// SPDX-License-Identifier: GPL-3.0-or-later
use ftdecode::{
    audio_input::{Channel, ConvertedWav},
    engine::{DecodeSettings, Mode},
    output::{EventOutput, OutputFormat, OutputOptions},
    stream,
    wav::WavReader,
    wav_cli::{WavFrames, seconds_offset, source_offset},
};
use serde_json::json;
use std::{
    env,
    fs::File,
    io::{self, BufReader, Write},
    net::{Shutdown, SocketAddr, TcpListener},
    time::Duration,
};

const HELP: &str = "ftdecode — standalone FT8/FT4 decoder\n\n\
  ftdecode decode FILE.wav [--mode ft8|ft4] [OPTIONS]\n\
  ftdecode serve [--bind 127.0.0.1:PORT] [--once]\n\
  ftdecode stdio\n\n\
WAV: RIFF/WAVE mono/stereo PCM 8/16/24/32-bit or float32, 12000..192000 Hz.\n\
Float WAV samples must be finite and in [-1,1].\n\
TCP/stdio: mono signed 16-bit PCM, 12000 samples/second.\n\
WAV results default to readable text; use --output jsonl for scripts. TCP/stdio use framed protocol v1.\n\
Decode options:\n\
  --mode ft8|ft4                  default: ft8\n\
  --min-hz HZ --max-hz HZ         default: 200..3000\n\
  --rx-freq HZ                    RX audio frequency; full band remains active\n\
  --priority-hz HZ                compatibility alias for --rx-freq\n\
  --priority-tolerance-hz HZ      default: 10\n\
  --ap off|cq|auto                default: off; auto uses context AP plus FT8 learned history\n\
  --my-call CALL --dx-call CALL   optional contact hints\n\
  --dx-grid GRID                  optional other-station locator\n\
  --qso-state STATE               calling-cq, calling-station, report, roger-report, rogers, signoff\n\
  --activity NAME                 normal, na-vhf, eu-vhf, field-day, rtty, ww-digi, fox, hound, arrl-digi\n\
  --ap-width-hz HZ                targeting half-width (default: 50)\n\
  --tx-freq HZ                    FT8-only second contact-AP audio target\n\
  --known-call CALL               seed hash resolution; repeat up to 400 calls\n\
  --depth 1|2|3                   default: 3\n\
  --threads N                     FT8 workers 1..12 (default: 1); FT4: 1\n\
  --slot-offset-samples N         first slot boundary in source sample frames\n\
  --slot-offset-seconds S         first slot boundary in decimal seconds\n\
  --start-utc-ns N                UTC nanoseconds at sample zero\n\
  --channel left|right|mix        stereo selection (default: left)\n\
  --output text|jsonl             result format (default: text)\n\
  -v, --verbose / -vv             message diagnostics / detailed diagnostics\n\
  --fields LIST                  comma-separated text columns in output order\n\
  --stats                        per-slot decode statistics\n\
  --no-slot-separators            disable separators between text slots (default: on)\n\
  --quiet                        suppress routine text diagnostics\n\n\
Fields: time,mode,snr,dt,freq,message,type,fec,bp_iterations,osd_pass,\n\
        hard_errors,pass,hashes,payload,assisted,ap_type,confidence,questionable,\n\
        grid,window_start_sample\n\n\
FT4 AP requires depth 2 or 3; contact AP uses --rx-freq, not --tx-freq.\n\
A single prepared window assumes sample zero is a slot boundary. Longer recordings require\n\
--slot-offset-samples, --slot-offset-seconds or --start-utc-ns.\n\
FT8: 15 s slots/windows; FT4: 7.5 s slots, 6.048 s decode windows.\n\
Incomplete tails are skipped; exit success does not guarantee a completed window.\n\
Serve handles one client at a time and defaults to 127.0.0.1:7373; port 0 chooses a free port. The bound address\n\
is reported on stderr. --once exits after the first client disconnects.\n";

fn parse<T: std::str::FromStr>(value: &str, label: &str) -> Result<T, String> {
    value
        .parse()
        .map_err(|_| format!("invalid {label}: {value}"))
}
fn next<'a>(args: &'a [String], i: &mut usize) -> Result<&'a str, String> {
    *i += 1;
    args.get(*i)
        .map(String::as_str)
        .ok_or_else(|| "missing option value".into())
}

fn decode(args: &[String]) -> Result<(), String> {
    let path = args
        .first()
        .filter(|x| !x.starts_with('-'))
        .ok_or("decode requires a WAV path")?;
    let mut mode = Mode::Ft8;
    let mut settings = DecodeSettings::default();
    let (mut utc, mut relative) = (None::<u64>, None::<u64>);
    let mut seconds = None;
    let mut channel = Channel::Left;
    let mut output_options = OutputOptions::default();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--mode" => {
                mode = match next(args, &mut i)? {
                    "ft8" => Mode::Ft8,
                    "ft4" => Mode::Ft4,
                    _ => return Err("mode must be ft8 or ft4".into()),
                }
            }
            "--min-hz" => settings.low_hz = parse(next(args, &mut i)?, "minimum frequency")?,
            "--max-hz" => settings.high_hz = parse(next(args, &mut i)?, "maximum frequency")?,
            "--rx-freq" | "--priority-hz" => {
                settings.priority_hz = Some(parse(next(args, &mut i)?, "priority frequency")?)
            }
            "--priority-tolerance-hz" => {
                settings.priority_tolerance_hz = parse(next(args, &mut i)?, "priority tolerance")?
            }
            "--ap" => {
                settings.ap.mode = parse(next(args, &mut i)?, "AP mode")?;
                settings.assistance = settings.ap.mode != ftdecode::assistance::ApMode::Off;
            }
            "--ap-width-hz" => settings.ap.width_hz = parse(next(args, &mut i)?, "AP width")?,
            "--tx-freq" => {
                settings.ap.tx_hz = Some(parse(next(args, &mut i)?, "TX audio frequency")?)
            }
            "--my-call" => {
                settings.ap.my_call = Some(next(args, &mut i)?.trim().to_ascii_uppercase())
            }
            "--dx-call" => {
                settings.ap.dx_call = Some(next(args, &mut i)?.trim().to_ascii_uppercase())
            }
            "--dx-grid" => settings.ap.dx_grid = Some(next(args, &mut i)?.to_ascii_uppercase()),
            "--qso-state" => settings.ap.state = parse(next(args, &mut i)?, "QSO state")?,
            "--activity" => {
                settings.ap.activity = parse(next(args, &mut i)?, "operating activity")?
            }
            "--known-call" => {
                if settings.ap.known_calls.len() >= 400 {
                    return Err("at most 400 known calls".into());
                }
                settings
                    .ap
                    .known_calls
                    .push(next(args, &mut i)?.trim().to_ascii_uppercase());
            }
            "--depth" => settings.depth = parse(next(args, &mut i)?, "depth")?,
            "--threads" => settings.threads = parse(next(args, &mut i)?, "threads")?,
            "--channel" => {
                channel = match next(args, &mut i)? {
                    "left" => Channel::Left,
                    "right" => Channel::Right,
                    "mix" => Channel::Mix,
                    _ => return Err("channel must be left, right, or mix".into()),
                };
            }
            "--output" => {
                output_options.format = match next(args, &mut i)? {
                    "text" => OutputFormat::Text,
                    "jsonl" => OutputFormat::Jsonl,
                    _ => return Err("output must be text or jsonl".into()),
                };
            }
            "-v" | "--verbose" => output_options.verbosity = (output_options.verbosity + 1).min(2),
            "-vv" => output_options.verbosity = 2,
            "--stats" => output_options.stats = true,
            "--no-slot-separators" => output_options.slot_separators = false,
            "--quiet" => output_options.quiet = true,
            "--fields" => {
                if output_options.fields.is_some() {
                    return Err("fields specified twice".into());
                }
                output_options.fields = Some(OutputOptions::parse_fields(next(args, &mut i)?)?);
            }
            "--slot-offset-seconds" => {
                if seconds.is_some() {
                    return Err("slot offset seconds specified twice".into());
                }
                seconds = Some(seconds_offset(next(args, &mut i)?)?);
            }
            "--slot-offset-samples" => {
                if relative.is_some() {
                    return Err("slot offset specified twice".into());
                }
                relative = Some(parse(next(args, &mut i)?, "slot offset")?);
            }
            "--start-utc-ns" => {
                if utc.is_some() {
                    return Err("UTC specified twice".into());
                }
                utc = Some(parse(next(args, &mut i)?, "UTC timestamp")?);
            }
            option => return Err(format!("unknown decode option: {option}")),
        }
        i += 1;
    }
    settings.validate()?;
    output_options.validate()?;
    if usize::from(utc.is_some()) + usize::from(relative.is_some()) + usize::from(seconds.is_some())
        > 1
    {
        return Err(
            "choose exactly one of --start-utc-ns, --slot-offset-samples, or --slot-offset-seconds"
                .into(),
        );
    }
    let wav = WavReader::new(BufReader::new(
        File::open(path).map_err(|e| format!("{path}: {e}"))?,
    ))
    .map_err(|e| e.to_string())?;
    let format = wav.format();
    let source_frames = wav.sample_count();
    relative = relative
        .map(|frames| source_offset(frames, format.sample_rate))
        .transpose()?
        .or(seconds);
    if utc.is_none() && relative.is_none() {
        if u128::from(source_frames) * 12000
            > mode.period_samples() as u128 * u128::from(format.sample_rate)
        {
            return Err("longer recordings require --slot-offset-samples, --slot-offset-seconds, or --start-utc-ns".into());
        }
        relative = Some(0);
    }
    let mut start = json!({"type":"start","payload_bytes":0,"version":1,"mode":mode.name(),"operation":"offline",
        "settings":{"low_hz":settings.low_hz,"high_hz":settings.high_hz,"priority_hz":settings.priority_hz,
                    "priority_tolerance_hz":settings.priority_tolerance_hz,"depth":settings.depth,"threads":settings.threads,"assistance":settings.assistance,"ap":settings.ap.mode.name(),
                    "ap_width_hz":settings.ap.width_hz,"tx_hz":settings.ap.tx_hz,
                    "my_call":settings.ap.my_call,"dx_call":settings.ap.dx_call,"dx_grid":settings.ap.dx_grid,
                    "qso_state":settings.ap.state.name(),"activity":settings.ap.activity.name(),"known_calls":settings.ap.known_calls}});
    if let Some(value) = utc {
        start["start_utc_ns"] = json!(value.to_string());
    }
    if let Some(value) = relative {
        start["slot_offset_samples"] = json!(value.to_string());
    }
    let wav = ConvertedWav::new(wav, channel).map_err(|e| e.to_string())?;
    let stream_options = stream::WavOptions {
        diagnostics: output_options.diagnostics_level(),
        stats: output_options.window_stats_enabled(),
    };
    let show_input = output_options.format == OutputFormat::Text
        || output_options.verbosity > 0
        || output_options.stats;
    let mut output = EventOutput::new(io::stdout(), io::stderr(), output_options);
    if show_input {
        output.event(&json!({"type":"input","source_rate":format.sample_rate,
            "channels":format.channels,"bits_per_sample":format.bits_per_sample,
            "valid_bits_per_sample":format.valid_bits_per_sample,
            "sample_format":match format.encoding { ftdecode::wav::SampleEncoding::PcmInteger => "PCM", ftdecode::wav::SampleEncoding::Float => "float" },
            "channel":if format.channels == 1 {"mono"} else {match channel {Channel::Left=>"left",Channel::Right=>"right",Channel::Mix=>"mix"}},
            "output_rate":12000,"source_frames":source_frames.to_string(),"output_frames":wav.sample_count().to_string()}))
            .map_err(|e|e.to_string())?;
    }
    let errors = output.error_flag();
    stream::serve_wav(WavFrames::new(wav, start), output, stream_options)
        .map_err(|e| e.to_string())?;
    if errors.load(std::sync::atomic::Ordering::Relaxed) {
        return Err("WAV decoding failed; see JSON error output".into());
    }
    Ok(())
}
fn serve(args: &[String]) -> Result<(), String> {
    let mut address: SocketAddr = "127.0.0.1:7373".parse().unwrap();
    let mut once = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--bind" => address = parse(next(args, &mut i)?, "listen address")?,
            "--once" => once = true,
            flag => return Err(format!("unknown serve option: {flag}")),
        }
        i += 1;
    }
    if !address.ip().is_loopback() {
        return Err("only localhost loopback addresses are supported".into());
    }
    let listener = TcpListener::bind(address).map_err(|e| e.to_string())?;
    eprintln!(
        "{}",
        json!({"type":"listening","address":listener.local_addr().map_err(|e|e.to_string())?.to_string()})
    );
    for accepted in listener.incoming() {
        let socket = accepted.map_err(|e| e.to_string())?;
        socket.set_nodelay(true).map_err(|e| e.to_string())?;
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| e.to_string())?;
        let reader = socket.try_clone().map_err(|e| e.to_string())?;
        let writer = socket.try_clone().map_err(|e| e.to_string())?;
        let result = stream::serve(reader, writer);
        let _ = socket.shutdown(Shutdown::Both);
        if let Err(error) = result {
            eprintln!(
                "{}",
                json!({"type":"connection_error","message":error.to_string()})
            );
        }
        if once {
            break;
        }
    }
    Ok(())
}
fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("--help" | "-h" | "help") => {
            print!("{HELP}");
            Ok(())
        }
        Some("--version" | "-V") => {
            println!("ftdecode {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("decode") => decode(&args[1..]),
        Some("serve") => serve(&args[1..]),
        Some("stdio") if args.len() == 1 => {
            stream::serve(io::stdin(), io::stdout()).map_err(|e| e.to_string())
        }
        Some(command) => Err(format!("unknown command or options: {command}; use --help")),
    }
}
fn main() {
    if let Err(error) = run() {
        let _ = writeln!(io::stderr(), "ftdecode: {error}");
        std::process::exit(2);
    }
}

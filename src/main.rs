use std::collections::VecDeque;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::path;
use std::time::SystemTime;

use clap::{value_t, App, Arg};
use console::style;
use env_logger::{Builder, Env};
use hound;
use indicatif::ProgressBar;
use serde_json::{json, Map};

mod ctx;

use golem_rpc_api::comp::{self, AsGolemComp};

const FLITE_JS: &[u8] = include_bytes!("../assets/flite.js");
const FLITE_WASM: &[u8] = include_bytes!("../assets/flite.wasm");
const DEFAULT_NUM_SUBTASKS: usize = 6;

static TRUCK: &str = "🚚  ";
static CLIP: &str = "🔗  ";
static PAPER: &str = "📃  ";
static HOURGLASS: &str = "⌛  ";

fn split_textfile(textfile: &str, num_subtasks: usize) -> Vec<String> {
    let mut reader = fs::File::open(textfile).unwrap();
    let mut contents = String::new();
    reader.read_to_string(&mut contents).unwrap();

    let word_count = contents.split_whitespace().count();

    println!(
        "{} {}Splitting '{}' into {} Golem subtasks...",
        style("[1/4]").bold().dim(),
        PAPER,
        textfile,
        num_subtasks,
    );

    let mut chunks: Vec<String> = Vec::with_capacity(num_subtasks);
    let num_words = (word_count as f64 / num_subtasks as f64).round() as usize;

    let mut acc: String = String::new();
    for (i, word) in contents.split_whitespace().enumerate() {
        acc.push_str(word);
        acc.push(' ');

        if (i + 1) % num_words == 0 {
            chunks.push(acc);
            acc = String::new();
            continue;
        }
    }

    if !acc.is_empty() {
        chunks.push(acc);
    }

    chunks
}

fn run_on_golem(chunks: Vec<String>, datadir: &str, address: &str, port: u16) -> VecDeque<String> {
    println!(
        "{} {}Sending task to Golem...",
        style("[2/4]").bold().dim(),
        TRUCK
    );

    // prepare workspace
    let mut workspace = env::temp_dir();
    let time_now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    let subdir = format!("g_flite_{}", time_now.as_secs());
    workspace.push(subdir);
    fs::create_dir(workspace.as_path()).unwrap();

    let mut input_dir = path::PathBuf::from(workspace.as_path());
    input_dir.push("in");
    fs::create_dir(input_dir.as_path()).unwrap();

    let mut output_dir = path::PathBuf::from(workspace.as_path());
    output_dir.push("out");
    fs::create_dir(output_dir.as_path()).unwrap();

    let mut js = path::PathBuf::from(input_dir.as_path());
    js.push("flite.js");
    let mut f = fs::File::create(js).unwrap();
    f.write_all(FLITE_JS).unwrap();

    let mut wasm = path::PathBuf::from(input_dir.as_path());
    wasm.push("flite.wasm");
    let mut f = fs::File::create(wasm).unwrap();
    f.write_all(FLITE_WASM).unwrap();

    let mut subtasks_map = Map::new();
    let mut wavefiles = VecDeque::new();

    for (i, chunk) in chunks.into_iter().enumerate() {
        let mut subtask_input = path::PathBuf::from(input_dir.as_path());
        let subtask_name = format!("subtask{}", i);

        subtask_input.push(&subtask_name);
        fs::create_dir(subtask_input.as_path()).unwrap();

        subtask_input.push("in.txt");
        let mut f = fs::File::create(subtask_input).unwrap();
        f.write_all(chunk.as_bytes()).unwrap();

        let mut subtask_output = path::PathBuf::from(output_dir.as_path());
        subtask_output.push(&subtask_name);
        fs::create_dir(subtask_output.as_path()).unwrap();

        subtasks_map.insert(
            subtask_name.clone(),
            json!({
                "exec_args": ["in.txt", "in.wav"],
                "output_file_paths": ["in.wav"],
            }),
        );

        subtask_output.push("in.wav");
        wavefiles.push_back(subtask_output.to_str().unwrap().to_string());
    }

    let task_json = json!({
        "type": "wasm",
        "name": "g_flite",
        "bid": 1,
        "subtask_timeout": "00:10:00",
        "timeout": "00:10:00",
        "options": {
            "js_name": "flite.js",
            "wasm_name": "flite.wasm",
            "input_dir": input_dir.to_str().unwrap(),
            "output_dir": output_dir.to_str().unwrap(),
            "subtasks": subtasks_map,
        }
    });

    let mut input_json = path::PathBuf::from(workspace.as_path());
    input_json.push("task.json");
    let f = fs::File::create(input_json.as_path()).unwrap();
    serde_json::to_writer_pretty(f, &task_json).unwrap();

    let mut ctx = ctx::CliCtx {
        rpc_addr: (address.into(), port),
        data_dir: path::PathBuf::from(datadir),
        json_output: true,
    };

    // send to Golem
    let (mut sys, endpoint) = ctx.connect_to_app().unwrap();
    let resp = sys
        .block_on(endpoint.as_golem_comp().create_task(task_json))
        .unwrap();
    let task_id = resp.0.unwrap();

    // wait
    println!(
        "{} {}Waiting on compute to finish...",
        style("[3/4]").bold().dim(),
        HOURGLASS
    );
    let num_tasks = wavefiles.len() as u64;
    let bar = ProgressBar::new(num_tasks);
    bar.inc(0);
    let mut old_progress = 0.0;

    loop {
        let resp = sys
            .block_on(endpoint.as_golem_comp().get_task(task_id.clone()))
            .unwrap();
        let task_info = resp.unwrap();
        let progress = task_info.progress.as_f64().unwrap() * 100.0;

        if progress != old_progress {
            let delta = (progress - old_progress) / 100.0;
            old_progress = progress;
            bar.inc((delta * num_tasks as f64).round() as u64);
        }

        match task_info.status {
            comp::TaskStatus::Finished => break,
            _ => {}
        }
    }

    wavefiles
}

fn combine_wave(mut wavefiles: VecDeque<String>, output_wavefile: &str) {
    if wavefiles.is_empty() {
        return;
    }

    println!(
        "{} {}Combining output into '{}'...",
        style("[4/4]").bold().dim(),
        CLIP,
        output_wavefile
    );

    let first = wavefiles.pop_front().unwrap();
    let reader = hound::WavReader::open(first).unwrap();
    let spec = reader.spec();
    let mut writer = hound::WavWriter::create(output_wavefile, spec).unwrap();
    for sample in reader.into_samples::<i16>() {
        writer.write_sample(sample.unwrap()).unwrap();
    }

    for wavefile in wavefiles {
        let reader = hound::WavReader::open(wavefile).unwrap();
        for sample in reader.into_samples::<i16>() {
            writer.write_sample(sample.unwrap()).unwrap();
        }
    }
}

fn main() {
    let matches = App::new("g_flite")
        .version("0.1.0")
        .author("Jakub Konka <jakub.konka@golem.network>")
        .about("flite, a text-to-speech program, distributed over Golem network")
        .arg(
            Arg::with_name("TEXTFILE")
                .help("Input text file")
                .required(true)
                .index(1),
        )
        .arg(
            Arg::with_name("WAVFILE")
                .help("Output WAV file")
                .required(true)
                .index(2),
        )
        .arg(
            Arg::with_name("subtasks")
                .long("subtasks")
                .value_name("NUM")
                .help("Sets number of Golem subtasks")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("datadir")
                .long("datadir")
                .value_name("DATADIR")
                .help("Sets path to Golem datadir")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("address")
                .long("address")
                .value_name("ADDRESS")
                .help("Sets RPC address to Golem instance")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("port")
                .long("port")
                .value_name("PORT")
                .help("Sets RPC port to Golem instance")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("verbose")
                .long("verbose")
                .short("v")
                .help("Turns verbose logging on")
                .takes_value(false),
        )
        .get_matches();

    let subtasks = value_t!(matches.value_of("subtasks"), usize).unwrap_or(DEFAULT_NUM_SUBTASKS);
    let datadir = matches.value_of("datadir").unwrap_or("~/datadir1/rinkeby");
    let address = matches.value_of("address").unwrap_or("127.0.0.1");
    let port = value_t!(matches.value_of("port"), u16).unwrap_or(61000);

    if matches.is_present("verbose") {
        Builder::from_env(Env::default().default_filter_or("debug")).init();
    }

    let chunks = split_textfile(matches.value_of("TEXTFILE").unwrap(), subtasks);
    let wavefiles = run_on_golem(chunks, datadir, address, port);
    combine_wave(wavefiles, matches.value_of("WAVFILE").unwrap());
}

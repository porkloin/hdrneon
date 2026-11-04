//! hdrneon: neon tubes on a dark wall above a strip of reference patches,
//! presented as SDR, HDR10 or scRGB. See the README for the patches.

mod gpu;

use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

/// How the frame reaches the display: the swapchain format and colour space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
	/// sRGB with 203 nits as white; anything brighter clips.
	Sdr,
	/// 10-bit BT.2020 PQ (`HDR10_ST2084`).
	Hdr10,
	/// 16-bit float linear BT.709, 1.0 = 80 nits (`EXTENDED_SRGB_LINEAR`).
	Scrgb,
}

impl Mode {
	const ALL: [Self; 3] = [Self::Sdr, Self::Hdr10, Self::Scrgb];

	fn name(self) -> &'static str {
		match self {
			Self::Sdr => "sdr",
			Self::Hdr10 => "hdr10",
			Self::Scrgb => "scrgb",
		}
	}

	fn next(self) -> Self {
		match self {
			Self::Sdr => Self::Hdr10,
			Self::Hdr10 => Self::Scrgb,
			Self::Scrgb => Self::Sdr,
		}
	}
}

pub struct Options {
	pub mode: Mode,
	pub cycle: Option<Duration>,
	pub duration: Option<Duration>,
	pub fullscreen: bool,
	pub size: (u32, u32),
	pub peak: f32,
	pub load: u32,
	pub vsync: bool,
	pub list_formats: bool,
	pub log: Option<String>,
}

const USAGE: &str = "\
usage: hdrneon [options]

  --mode sdr|hdr10|scrgb  swapchain to present to (default hdr10)
  --cycle SECONDS         switch to the next mode every SECONDS (sdr -> hdr10 -> scrgb)
  --duration SECONDS      exit after SECONDS
  --fullscreen            borderless fullscreen
  --size WxH              window size in pixels (default 1920x1080)
  --peak NITS             brightness of the neon cores (default 1000)
  --load N                extra shader work per pixel (default 0)
  --no-vsync              present as fast as possible (mailbox or immediate)
  --list-formats          print the surface formats and exit
  --log PATH              append the mode presented, and each switch, as JSON lines to PATH

keys: m or space switches mode, q or escape quits. frame times are printed on exit.";

fn parse_args() -> Result<Options, String> {
	let mut o = Options {
		mode: Mode::Hdr10,
		cycle: None,
		duration: None,
		fullscreen: false,
		size: (1920, 1080),
		peak: 1000.0,
		load: 0,
		vsync: true,
		list_formats: false,
		log: None,
	};
	let mut args = std::env::args().skip(1);
	let secs = |v: Option<String>, flag: &str| -> Result<Duration, String> {
		v.and_then(|v| v.parse::<f64>().ok())
			.filter(|s| *s > 0.0)
			.map(Duration::from_secs_f64)
			.ok_or_else(|| format!("{flag} needs a number of seconds"))
	};
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--mode" => {
				let v = args.next().unwrap_or_default();
				o.mode = Mode::ALL
					.into_iter()
					.find(|m| m.name() == v)
					.ok_or_else(|| format!("unknown mode {v:?}"))?;
			},
			"--cycle" => o.cycle = Some(secs(args.next(), "--cycle")?),
			"--log" => o.log = Some(args.next().ok_or("--log needs a path")?),
			"--duration" => o.duration = Some(secs(args.next(), "--duration")?),
			"--fullscreen" => o.fullscreen = true,
			"--size" => {
				let v = args.next().unwrap_or_default();
				o.size = v
					.split_once('x')
					.and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
					.ok_or_else(|| format!("--size needs WxH, got {v:?}"))?;
			},
			"--peak" => {
				o.peak = args
					.next()
					.and_then(|v| v.parse().ok())
					.filter(|n: &f32| (1.0..=10000.0).contains(n))
					.ok_or("--peak needs nits between 1 and 10000")?;
			},
			"--load" => {
				o.load = args
					.next()
					.and_then(|v| v.parse().ok())
					.ok_or("--load needs a number")?
			},
			"--no-vsync" => o.vsync = false,
			"--list-formats" => o.list_formats = true,
			"-h" | "--help" => {
				println!("{USAGE}");
				std::process::exit(0);
			},
			_ => return Err(format!("unknown option {arg:?}")),
		}
	}
	Ok(o)
}

struct App {
	options: Options,
	gpu: Option<gpu::Gpu>,
	start: Instant,
	mode_since: Instant,
	last_frame: Option<Instant>,
	frame_times: Vec<f32>,
	error: Option<String>,
	/// --log, appended to so a reader following it keeps its place when hdrneon starts again.
	log: Option<std::fs::File>,
}

/// Microseconds since the Unix epoch.
fn unix_us() -> u128 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map_or(0, |d| d.as_micros())
}

impl App {
	/// One line of --log, written whole and flushed: the mode now presented, and whether it's a switch.
	fn log_mode(&mut self, mode: Mode, switch: bool) {
		use std::io::Write;
		if let Some(log) = &mut self.log
			&& let Err(e) = writeln!(
				log,
				r#"{{"event":"mode","mode":"{}","switch":{switch},"unix_us":{}}}"#,
				mode.name(),
				unix_us()
			) {
			eprintln!("hdrneon: writing the log failed: {e}");
			self.log = None;
		}
	}

	fn fail(&mut self, event_loop: &ActiveEventLoop, error: String) {
		self.error = Some(error);
		event_loop.exit();
	}

	/// Switch to the next mode the display can show, saying why for any it
	/// skips. With none, it stays in the current one.
	fn next_mode(&mut self) {
		let Some(gpu) = &mut self.gpu else { return };
		let mut mode = gpu.mode();
		let mut switched = None;
		for _ in 1..Mode::ALL.len() {
			mode = mode.next();
			match gpu.set_mode(mode) {
				Ok(()) => {
					eprintln!("hdrneon: {}", mode.name());
					switched = Some(mode);
					break;
				},
				Err(e) => eprintln!("hdrneon: {e}"),
			}
		}
		if let Some(mode) = switched {
			self.log_mode(mode, true);
		}
		self.mode_since = Instant::now();
	}

	fn frame(&mut self, event_loop: &ActiveEventLoop) {
		let now = Instant::now();
		if self.options.duration.is_some_and(|d| now - self.start >= d) {
			event_loop.exit();
			return;
		}
		if self.options.cycle.is_some_and(|c| now - self.mode_since >= c) {
			self.next_mode();
		}
		let Some(gpu) = &mut self.gpu else { return };
		if let Err(e) = gpu.draw((now - self.start).as_secs_f32()) {
			self.fail(event_loop, e);
			return;
		}
		if let Some(last) = self.last_frame.replace(now) {
			self.frame_times.push((now - last).as_secs_f32() * 1000.0);
		}
	}

	fn report(&mut self) {
		let t = &mut self.frame_times;
		let Some(&max) = t.iter().max_by(|a, b| a.total_cmp(b)) else {
			return;
		};
		t.sort_by(f32::total_cmp);
		let at = |q: f32| t[((t.len() - 1) as f32 * q) as usize];
		eprintln!(
			"hdrneon: {} frames, {:.1} fps, frame time p50 {:.2} ms, p99 {:.2} ms, max {:.2} ms",
			t.len(),
			t.len() as f32 * 1000.0 / t.iter().sum::<f32>(),
			at(0.5),
			at(0.99),
			max,
		);
	}
}

impl ApplicationHandler for App {
	fn resumed(&mut self, event_loop: &ActiveEventLoop) {
		if self.gpu.is_some() {
			return;
		}
		let (w, h) = self.options.size;
		let mut attributes = Window::default_attributes()
			.with_title("hdrneon")
			.with_inner_size(PhysicalSize::new(w, h));
		if self.options.fullscreen {
			attributes = attributes.with_fullscreen(Some(Fullscreen::Borderless(None)));
		}
		let window = match event_loop.create_window(attributes) {
			Ok(window) => Arc::new(window),
			Err(e) => return self.fail(event_loop, format!("couldn't create a window: {e}")),
		};
		match gpu::Gpu::new(window, &self.options) {
			Ok(gpu) if self.options.list_formats => {
				gpu.print_formats();
				event_loop.exit();
			},
			Ok(gpu) => {
				eprintln!("hdrneon: {} on {}", gpu.mode().name(), gpu.device_name());
				let mode = gpu.mode();
				self.gpu = Some(gpu);
				self.log_mode(mode, false);
				self.start = Instant::now();
				self.mode_since = self.start;
			},
			Err(e) => self.fail(event_loop, e),
		}
	}

	fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
		match event {
			WindowEvent::CloseRequested => event_loop.exit(),
			WindowEvent::Resized(_) => {
				if let Some(gpu) = &mut self.gpu {
					gpu.resized();
				}
			},
			WindowEvent::KeyboardInput {
				event: KeyEvent {
					logical_key,
					state: ElementState::Pressed,
					repeat: false,
					..
				},
				..
			} => match logical_key.as_ref() {
				Key::Named(NamedKey::Escape) | Key::Character("q") => event_loop.exit(),
				Key::Named(NamedKey::Space) | Key::Character("m") => self.next_mode(),
				_ => {},
			},
			WindowEvent::RedrawRequested => self.frame(event_loop),
			_ => {},
		}
	}

	fn about_to_wait(&mut self, _: &ActiveEventLoop) {
		if let Some(gpu) = &self.gpu {
			gpu.window().request_redraw();
		}
	}
}

fn main() -> ExitCode {
	let options = match parse_args() {
		Ok(o) => o,
		Err(e) => {
			eprintln!("hdrneon: {e}\n\n{USAGE}");
			return ExitCode::from(2);
		},
	};
	let event_loop = match EventLoop::new() {
		Ok(l) => l,
		Err(e) => {
			eprintln!("hdrneon: no display: {e}");
			return ExitCode::FAILURE;
		},
	};
	let mut app = App {
		options,
		gpu: None,
		start: Instant::now(),
		mode_since: Instant::now(),
		last_frame: None,
		frame_times: Vec::new(),
		error: None,
		log: None,
	};
	if let Some(path) = &app.options.log {
		match std::fs::OpenOptions::new().create(true).append(true).open(path) {
			Ok(f) => app.log = Some(f),
			Err(e) => {
				eprintln!("hdrneon: {path}: {e}");
				return ExitCode::FAILURE;
			},
		}
	}
	if let Err(e) = event_loop.run_app(&mut app) {
		eprintln!("hdrneon: {e}");
		return ExitCode::FAILURE;
	}
	app.report();
	match app.error {
		Some(e) => {
			eprintln!("hdrneon: {e}");
			ExitCode::FAILURE
		},
		None => ExitCode::SUCCESS,
	}
}

use clap::Parser;
use dashmap::DashMap;
use eframe::egui::{self, UserData, Vec2b};
use egui_plot::{Corner, FilledArea, Legend, Line, Plot, PlotPoints};
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use tfrecord::{EventIter, protobuf::event::What, protobuf::summary::value::Value::SimpleValue};
#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Scalarboard: A high-performance TensorBoard-like viewer in Rust for scalars"
)]
struct Args {
    /// The root directory containing tfevents files
    #[arg(short, long, default_value = ".")]
    path: String,

    /// Maximum recursion depth for finding files
    #[arg(short, long, default_value_t = 2)]
    depth: usize,
}
struct ScalarPoint {
    run_name: Arc<str>,
    tag: Arc<str>,
    step: u32,
    value: f32,
}

#[derive(Default, Debug, Clone)]
struct MyPlotPoint {
    step: u32,
    value: f32,
}

#[derive(Default, Debug, Clone)]
struct BinnedData {
    means: Vec<f32>,
    lowers: Vec<f64>,
    uppers: Vec<f64>,
    m2: Vec<f32>,
    num_elements: Vec<u32>,
    x_coords: Vec<f64>,         // Pre-calculated x-axis values
    line_coords: Vec<[f64; 2]>, // Pre-zipped [x, y] for Line
    processed_upto: usize,
}

enum ControlMsg {
    NewPoint(ScalarPoint),
    ResetInterval(u32),
}

impl BinnedData {
    fn update_incrementally(&mut self, raw_data: &[MyPlotPoint], bin_width: u32) {
        if raw_data.is_empty() || self.processed_upto >= raw_data.len() {
            return;
        }

        let last_step = raw_data.last().unwrap().step;
        let required_bins = ((last_step / bin_width) + 1) as usize;

        if self.means.len() < required_bins {
            self.means.resize(required_bins, 0.0);
            self.lowers.resize(required_bins, 0.0);
            self.uppers.resize(required_bins, 0.0);
            self.m2.resize(required_bins, 0.0);
            self.num_elements.resize(required_bins, 0);
            self.x_coords.resize(required_bins, 0.0);
            self.line_coords.resize(required_bins, [0.0, 0.0]);

            let interval = bin_width as f64;
            for i in 0..required_bins {
                self.x_coords[i] = (i as f64 * interval) + (interval / 2.0);
            }
        }

        // Process the new raw points into their respective bins
        for i in self.processed_upto..raw_data.len() {
            let p = &raw_data[i];
            let bin_idx = (p.step / bin_width) as usize;

            self.num_elements[bin_idx] += 1;
            let n = self.num_elements[bin_idx] as f32;

            let delta = p.value - self.means[bin_idx];
            self.means[bin_idx] += delta / n;
            let delta2 = p.value - self.means[bin_idx];
            self.m2[bin_idx] += delta * delta2;

            let std_dev = (self.m2[bin_idx] / n).sqrt();
            self.uppers[bin_idx] = (self.means[bin_idx] + std_dev).into();
            self.lowers[bin_idx] = (self.means[bin_idx] - std_dev).into();
            self.line_coords[bin_idx] = [self.x_coords[bin_idx], self.means[bin_idx] as f64];
        }

        // If a bin is empty, inherit the value from the previous bin
        for i in 1..required_bins {
            if self.num_elements[i] == 0 {
                self.means[i] = self.means[i - 1];
                self.lowers[i] = self.lowers[i - 1];
                self.uppers[i] = self.uppers[i - 1];
                self.line_coords[i] = [self.x_coords[i], self.means[i] as f64];
            }
        }

        self.processed_upto = raw_data.len();
    }
}

fn start_compute_worker(
    receiver: Receiver<ControlMsg>,
    cache: Arc<DashMap<Arc<str>, DashMap<Arc<str>, BinnedData>>>,
    is_processing: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let mut raw_store: BTreeMap<Arc<str>, BTreeMap<Arc<str>, Vec<MyPlotPoint>>> =
            BTreeMap::new();
        let mut current_interval = 10_000;
        let mut pending_tags: HashSet<Arc<str>> = HashSet::new();

        loop {
            let first_msg = match receiver.recv() {
                Ok(m) => m,
                Err(_) => break,
            };

            let mut messages = vec![first_msg];
            while let Ok(extra) = receiver.try_recv() {
                messages.push(extra);
            }

            let mut reset_requested = false;

            for msg in messages {
                match msg {
                    ControlMsg::NewPoint(p) => {
                        raw_store
                            .entry(p.tag.clone())
                            .or_default()
                            .entry(p.run_name.clone())
                            .or_default()
                            .push(MyPlotPoint {
                                step: p.step,
                                value: p.value,
                            });
                        pending_tags.insert(p.tag);
                    }
                    ControlMsg::ResetInterval(new_interval) => {
                        current_interval = new_interval;
                        reset_requested = true;
                    }
                }
            }

            if reset_requested {
                cache.clear();
                for (tag, runs) in &raw_store {
                    for (run_name, points) in runs {
                        let tag_entry = cache.entry(tag.clone()).or_default();
                        let mut binned = tag_entry.value().entry(run_name.clone()).or_default();
                        binned.update_incrementally(points, current_interval);
                    }
                }
                is_processing.store(false, Ordering::SeqCst);
            } else {
                for tag in pending_tags.drain() {
                    if let Some(runs) = raw_store.get(&tag) {
                        for (run_name, points) in runs {
                            let tag_entry = cache.entry(tag.clone()).or_default();
                            let mut binned = tag_entry.value().entry(run_name.clone()).or_default();
                            binned.update_incrementally(points, current_interval);
                        }
                    }
                }
            }
        }
    });
}

// --- FILE SYSTEM HELPERS ---

fn process_new_events(
    path: &PathBuf,
    offsets: &mut HashMap<PathBuf, u64>,
    tx: &Sender<ScalarPoint>,
) {
    let last_offset = *offsets.get(path).unwrap_or(&0);
    if let Ok(mut file) = File::open(path) {
        let current_len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if current_len > last_offset {
            let _ = file.seek(SeekFrom::Start(last_offset));
            let reader = EventIter::from_reader(file, Default::default());
            let run_name: Arc<str> = path
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into())
                .unwrap_or_else(|| "root".into());

            for result in reader.flatten() {
                if let Some(What::Summary(summary)) = result.what {
                    for val in summary.value {
                        if let Some(SimpleValue(v)) = val.value {
                            let _ = tx.send(ScalarPoint {
                                run_name: run_name.clone(),
                                tag: val.tag.into(),
                                step: result.step as u32,
                                value: v,
                            });
                        }
                    }
                }
            }
            offsets.insert(path.clone(), current_len);
        }
    }
}

fn start_live_monitor(
    root_path: PathBuf,
    tx: Sender<ScalarPoint>,
    offsets: Arc<Mutex<HashMap<PathBuf, u64>>>,
) {
    let (event_tx, event_rx) = channel();
    let mut watcher = RecommendedWatcher::new(event_tx, Config::default()).expect("Watcher fail");
    watcher
        .watch(&root_path, RecursiveMode::Recursive)
        .expect("Watch path fail");
    println!("Watching {}", root_path.display());

    thread::spawn(move || {
        // Keep watcher alive by moving it into the thread
        let _watcher = watcher;
        for res in event_rx {
            if let Ok(event) = res {
                if let EventKind::Modify(_) = event.kind {
                    for path in event.paths {
                        if path.to_string_lossy().contains("tfevents") {
                            let mut offsets_map = offsets.lock().unwrap();
                            // println!("New event in file {}", path.display());
                            process_new_events(&path, &mut *offsets_map, &tx);
                        }
                    }
                }
            }
        }
    });
}

fn visit_dirs(
    dir: &Path,
    tx: &Sender<ScalarPoint>,
    depth: usize,
    max_depth: usize,
    offsets: &Arc<Mutex<HashMap<PathBuf, u64>>>,
    handles: &mut Vec<thread::JoinHandle<()>>,
) {
    if depth > max_depth {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit_dirs(&path, tx, depth + 1, max_depth, offsets, handles);
            } else if path.is_file() && path.to_string_lossy().contains("tfevents") {
                let tx_file = tx.clone();
                let offsets_file = Arc::clone(offsets);
                let handle = thread::spawn(move || {
                    if let Ok(reader) = EventIter::open(&path, Default::default()) {
                        let run_name: Arc<str> = path
                            .parent()
                            .and_then(|p| p.file_name())
                            .map(|n| n.to_string_lossy().into())
                            .unwrap_or_else(|| "root".into());
                        for result in reader.flatten() {
                            if let Some(What::Summary(summary)) = result.what {
                                for val in summary.value {
                                    if let Some(SimpleValue(v)) = val.value {
                                        let _ = tx_file.send(ScalarPoint {
                                            run_name: run_name.clone(),
                                            tag: val.tag.into(),
                                            step: result.step as u32,
                                            value: v,
                                        });
                                    }
                                }
                            }
                        }
                        if let Ok(meta) = std::fs::metadata(&path) {
                            offsets_file.lock().unwrap().insert(path, meta.len());
                        }
                    }
                });
                handles.push(handle);
            }
        }
    }
}

// --- UI APPLICATION ---

struct RLApp {
    bin_cache: Arc<DashMap<Arc<str>, DashMap<Arc<str>, BinnedData>>>,
    worker_tx: Sender<ControlMsg>,
    raw_receiver: Receiver<ScalarPoint>,
    maximized_tag: Option<Arc<str>>,
    step_interval: u32,
    is_processing: Arc<AtomicBool>,
}
fn get_color_for_run(run_name: &str) -> egui::Color32 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(run_name, &mut hasher);
    let hash = std::hash::Hasher::finish(&hasher);

    // Generate a color using the hash
    // We use a golden angle approach or a simple palette selection
    let h = (hash % 360) as f32 / 360.0;
    let s = 0.6; // Keep saturation and value constant for look-and-feel
    let v = 0.8;

    // Simple HSV to RGB conversion
    let c = v * s;
    let x = c * (1.0 - ((h * 6.0) % 2.0 - 1.0).abs());
    let m = v - c;

    let (r, g, b) = if h < 1.0 / 6.0 {
        (c, x, 0.0)
    } else if h < 2.0 / 6.0 {
        (x, c, 0.0)
    } else if h < 3.0 / 6.0 {
        (0.0, c, x)
    } else if h < 4.0 / 6.0 {
        (0.0, x, c)
    } else if h < 5.0 / 6.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };

    egui::Color32::from_rgb(
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}
impl RLApp {
    fn new(_cc: &eframe::CreationContext<'_>, root_path: String, max_depth: usize) -> Self {
        let (raw_tx, raw_rx) = channel();
        let (worker_tx, worker_rx) = channel();
        let shared_cache = Arc::new(DashMap::new());
        let is_processing = Arc::new(AtomicBool::new(false));

        start_compute_worker(
            worker_rx,
            Arc::clone(&shared_cache),
            Arc::clone(&is_processing),
        );

        let root = PathBuf::from(root_path);
        let root_clone = root.clone();
        let offsets = Arc::new(Mutex::new(HashMap::new()));

        let tx_scan = raw_tx.clone();
        let offsets_scan = Arc::clone(&offsets);
        thread::spawn(move || {
            let mut handles = vec![];
            visit_dirs(&root, &tx_scan, 0, max_depth, &offsets_scan, &mut handles);
            for h in handles {
                let _ = h.join();
            }
            start_live_monitor(root_clone, tx_scan, offsets_scan);
        });

        Self {
            bin_cache: shared_cache,
            worker_tx,
            raw_receiver: raw_rx,
            is_processing,
            maximized_tag: None,
            step_interval: 10_000,
        }
    }

    fn draw_plot_contents(&self, plot_ui: &mut egui_plot::PlotUi, tag: &str) {
        // let cache = self.bin_cache.lock().unwrap();

        if let Some(runs) = self.bin_cache.get(tag) {
            let mut sorted_runs: Vec<_> = runs.iter().collect();
            sorted_runs.sort_by(|a, b| a.key().cmp(b.key()));
            for entry in sorted_runs.iter() {
                let run_name = entry.key();
                let binned = entry.value();
                if binned.means.is_empty() {
                    continue;
                }
                let base_color = get_color_for_run(run_name);
                plot_ui.add(
                    FilledArea::new(
                        format!("{}_area", run_name),
                        &binned.x_coords,
                        &binned.lowers,
                        &binned.uppers,
                    )
                    .fill_color(base_color.linear_multiply(0.15)),
                );

                let line_points = PlotPoints::new(binned.line_coords.clone());
                plot_ui.line(
                    Line::new(run_name.to_string(), line_points)
                        .name(run_name.to_string())
                        .color(base_color),
                );
            }
        }
    }

    fn render_thumbnail_plot(&mut self, ui: &mut egui::Ui, tag: Arc<str>) {
        ui.allocate_ui(egui::vec2(450.0, 280.0), |ui| {
            ui.group(|ui| {
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.add(egui::Label::new(tag.as_ref()).truncate());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("⛶").clicked() {
                                self.maximized_tag = Some(tag.clone());
                            }
                        });
                    });
                    ui.add_space(4.0);
                    Plot::new(tag.as_ref())
                        .view_aspect(2.0)
                        .height(200.0)
                        .allow_drag(false)
                        .allow_zoom(false)
                        .legend(
                            Legend::default()
                                .position(Corner::RightBottom)
                                .text_style(egui::TextStyle::Small),
                        )
                        .link_axis("thumbnail_plots", Vec2b::new(true, false))
                        .show(ui, |plot_ui| self.draw_plot_contents(plot_ui, tag.as_ref()));
                });
            });
        });
    }
}

impl eframe::App for RLApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut screenshot_path = None;
        let mut should_repaint = false;
        while let Ok(p) = self.raw_receiver.try_recv() {
            let _ = self.worker_tx.send(ControlMsg::NewPoint(p));
            should_repaint = true;
        }
        if self.is_processing.load(Ordering::SeqCst) {
            should_repaint = true;
        }

        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
            ui.style_mut().spacing.slider_width = 300.0;
            ui.horizontal(|ui| {
                ui.label("Binning Interval:");
                let slider_res = ui.add(
                    egui::Slider::new(&mut self.step_interval, 300..=100000).logarithmic(true),
                );
                if slider_res.changed() || slider_res.dragged() {
                    should_repaint = true;
                }
                if slider_res.drag_stopped() {
                    self.is_processing.store(true, Ordering::SeqCst);
                    should_repaint = true;
                    let _ = self
                        .worker_tx
                        .send(ControlMsg::ResetInterval(self.step_interval));
                }
                if self.is_processing.load(Ordering::SeqCst) {
                    ui.add(egui::Spinner::new());
                    ui.weak("Processing...");
                }
                if self.maximized_tag.is_some() && ui.button("⬅ Back").clicked() {
                    self.maximized_tag = None;
                    should_repaint = true;
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(tag) = self.maximized_tag.clone() {
                if ui.button("📷").on_hover_text("Export PNG").clicked() {
                    screenshot_path = Some(
                        std::env::current_dir()
                            .unwrap()
                            .join(format!("{}.png", tag.replace("/", "_"))),
                    );
                    should_repaint = true;
                }
                Plot::new("full")
                    .legend(Legend::default())
                    .show(ui, |plot_ui| self.draw_plot_contents(plot_ui, tag.as_ref()));
            } else {
                let mut sections: BTreeMap<String, Vec<Arc<str>>> = BTreeMap::new();
                for r in self.bin_cache.iter() {
                    let tag = r.key();
                    let section = tag.split('/').next().unwrap_or("General").to_string();
                    sections.entry(section).or_default().push(tag.clone());
                }

                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.heading("RL-Board Desktop");
                    for (section_name, tags) in sections {
                        egui::CollapsingHeader::new(&section_name)
                            .default_open(true)
                            .show(ui, |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    for tag in tags {
                                        self.render_thumbnail_plot(ui, tag);
                                    }
                                });
                            });
                        ui.add_space(15.0);
                    }
                });
            }
        });

        if screenshot_path.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(UserData::default()));
        }

        ctx.input(|i| {
            for event in &i.raw.events {
                if let egui::Event::Screenshot { image, .. } = event {
                    save_screenshot(PathBuf::from("plot_export.png"), image);
                }
            }
        });
        if should_repaint {
            ctx.request_repaint();
        }
    }
}

fn save_screenshot(path: PathBuf, image: &Arc<egui::ColorImage>) {
    let size = image.size;
    if let Some(buffer) = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(
        size[0] as u32,
        size[1] as u32,
        image.as_raw().to_vec(),
    ) {
        let _ = buffer.save(&path);
    }
}

fn main() -> eframe::Result<()> {
    let args = Args::parse();

    eframe::run_native(
        "ScalarBoard Desktop",
        eframe::NativeOptions::default(),
        Box::new(|cc| Ok(Box::new(RLApp::new(cc, args.path, args.depth)))),
    )
}

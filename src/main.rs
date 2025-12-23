use eframe::egui::{self, UserData, Vec2b};
use egui_plot::{Corner, FilledArea, Legend, Line, Plot, PlotPoints};
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use tfrecord::{EventIter, protobuf::event::What, protobuf::summary::value::Value::SimpleValue};

struct ScalarPoint {
    run_name: String,
    tag: String,
    step: u32,
    value: f64,
}

#[derive(Default, Debug, Clone)]
struct MyPlotPoint {
    step: u32,
    value: f64,
}

#[derive(Default, Debug, Clone)]
struct BinnedData {
    means: Vec<f64>,
    lowers: Vec<f64>,
    uppers: Vec<f64>,
    m2: Vec<f64>,
    num_elements: Vec<u32>,
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
        }

        for i in self.processed_upto..raw_data.len() {
            let p = &raw_data[i];
            let bin_idx = (p.step / bin_width) as usize;

            self.num_elements[bin_idx] += 1;
            let n = self.num_elements[bin_idx] as f64;

            let delta = p.value - self.means[bin_idx];
            self.means[bin_idx] += delta / n;
            let delta2 = p.value - self.means[bin_idx];
            self.m2[bin_idx] += delta * delta2;

            if n > 0.0 {
                let std_dev = (self.m2[bin_idx] / n).sqrt();
                self.uppers[bin_idx] = self.means[bin_idx] + std_dev;
                self.lowers[bin_idx] = self.means[bin_idx] - std_dev;
            }
        }
        self.processed_upto = raw_data.len();
    }
}

// --- BACKGROUND COMPUTE WORKER ---

fn start_compute_worker(
    receiver: Receiver<ControlMsg>,
    shared_cache: Arc<Mutex<BTreeMap<String, BTreeMap<String, BinnedData>>>>,
    is_processing: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let mut raw_store: BTreeMap<String, BTreeMap<String, Vec<MyPlotPoint>>> = BTreeMap::new();
        let mut current_interval = 10_000;
        let mut pending_tags: std::collections::HashSet<String> = std::collections::HashSet::new();

        loop {
            // Wait for at least one message
            let first_msg = match receiver.recv() {
                Ok(m) => m,
                Err(_) => break, // Channel closed
            };

            // Collect all available messages in the buffer to process them at once
            let mut messages = vec![first_msg];
            while let Ok(extra) = receiver.try_recv() {
                messages.push(extra);
            }

            let mut reset_requested = false;

            for msg in messages {
                match msg {
                    ControlMsg::NewPoint(p) => {
                        let points = raw_store
                            .entry(p.tag.clone())
                            .or_default()
                            .entry(p.run_name.clone())
                            .or_default();
                        points.push(MyPlotPoint {
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

            // Update the UI-visible cache
            if let Ok(mut cache) = shared_cache.lock() {
                if reset_requested {
                    eprintln!(
                        "[Worker] Resetting cache for new interval: {}",
                        current_interval
                    );
                    cache.clear();
                    for (tag, runs) in &raw_store {
                        for (run_name, points) in runs {
                            let binned = cache
                                .entry(tag.clone())
                                .or_default()
                                .entry(run_name.clone())
                                .or_default();
                            binned.update_incrementally(points, current_interval);
                        }
                    }
                    is_processing.store(false, Ordering::SeqCst);
                } else {
                    // Only update tags that actually got new points
                    for tag in pending_tags.drain() {
                        if let Some(runs) = raw_store.get(&tag) {
                            for (run_name, points) in runs {
                                let binned = cache
                                    .entry(tag.clone())
                                    .or_default()
                                    .entry(run_name.clone())
                                    .or_default();
                                binned.update_incrementally(points, current_interval);
                            }
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
            let run_name = path
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "root".into());

            for result in reader.flatten() {
                if let Some(What::Summary(summary)) = result.what {
                    for val in summary.value {
                        if let Some(SimpleValue(v)) = val.value {
                            let _ = tx.send(ScalarPoint {
                                run_name: run_name.clone(),
                                tag: val.tag,
                                step: result.step as u32,
                                value: v as f64,
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
    root_path: &PathBuf,
    tx: Sender<ScalarPoint>,
    offsets: Arc<Mutex<HashMap<PathBuf, u64>>>,
) {
    let (event_tx, event_rx) = channel();
    let mut watcher = RecommendedWatcher::new(event_tx, Config::default()).expect("Watcher fail");
    watcher
        .watch(root_path, RecursiveMode::Recursive)
        .expect("Watch path fail");

    println!("Starting Live monitor");

    thread::spawn(move || {
        for res in event_rx {
            if let Ok(event) = res {
                if let EventKind::Modify(_) = event.kind {
                    for path in event.paths {
                        if path.to_string_lossy().contains("tfevents") {
                            let mut offsets_map = offsets.lock().unwrap();
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
                        let run_name = path
                            .parent()
                            .and_then(|p| p.file_name())
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "root".into());
                        for result in reader.flatten() {
                            if let Some(What::Summary(summary)) = result.what {
                                for val in summary.value {
                                    if let Some(SimpleValue(v)) = val.value {
                                        let _ = tx_file.send(ScalarPoint {
                                            run_name: run_name.clone(),
                                            tag: val.tag,
                                            step: result.step as u32,
                                            value: v as f64,
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

struct RLApp {
    // Shared binned data from worker
    bin_cache: Arc<Mutex<BTreeMap<String, BTreeMap<String, BinnedData>>>>,
    // Channel to send data to worker
    worker_tx: Sender<ControlMsg>,
    // Channel receiving from file watchers
    raw_receiver: Receiver<ScalarPoint>,

    maximized_tag: Option<String>,
    step_interval: u32,
    // Track processing state for UI
    is_processing: Arc<AtomicBool>,
}

impl RLApp {
    fn new(_cc: &eframe::CreationContext<'_>, root_path: String, max_depth: usize) -> Self {
        let (raw_tx, raw_rx) = channel();
        let (worker_tx, worker_rx) = channel();
        let shared_cache = Arc::new(Mutex::new(BTreeMap::new()));
        let is_processing = Arc::new(AtomicBool::new(false));

        // Start Compute Worker
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
            start_live_monitor(&root_clone, tx_scan, offsets_scan);
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
        let cache = self.bin_cache.lock().unwrap();
        let is_dark = plot_ui.ctx().style().visuals.dark_mode;
        let fill_color = if is_dark {
            egui::Color32::from_white_alpha(30) // Subtle white for dark mode
        } else {
            egui::Color32::from_black_alpha(25) // Subtle black for light mode
        };

        if let Some(runs) = cache.get(tag) {
            let interval = self.step_interval;
            for (run_name, binned) in runs {
                if binned.means.is_empty() {
                    continue;
                }

                let xs: Vec<f64> = (0..binned.means.len())
                    .map(|i| (i as f64 * interval as f64) + (interval as f64 / 2.0))
                    .collect();

                plot_ui.add(
                    FilledArea::new(
                        format!("{}_area", run_name),
                        &xs,
                        &binned.lowers,
                        &binned.uppers,
                    )
                    .fill_color(fill_color),
                );

                let line_points: PlotPoints = xs
                    .iter()
                    .zip(&binned.means)
                    .map(|(&x, &y)| [x, y])
                    .collect();
                plot_ui.line(Line::new(run_name, line_points).name(run_name));
            }
        }
    }

    fn render_thumbnail_plot(&mut self, ui: &mut egui::Ui, tag: &str) {
        ui.allocate_ui(egui::vec2(450.0, 280.0), |ui| {
            ui.group(|ui| {
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.add(egui::Label::new(tag).truncate());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("⛶").clicked() {
                                self.maximized_tag = Some(tag.to_string());
                            }
                        });
                    });
                    ui.add_space(4.0);
                    Plot::new(tag)
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
                        .show(ui, |plot_ui| self.draw_plot_contents(plot_ui, tag));
                });
            });
        });
    }
}

impl eframe::App for RLApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut screenshot_path = None;
        let mut new_points_count = 0;

        // Drain the file watcher channel
        while let Ok(p) = self.raw_receiver.try_recv() {
            new_points_count += 1;
            let _ = self.worker_tx.send(ControlMsg::NewPoint(p));
        }

        if new_points_count > 0 {
            eprintln!("[UI] Forwarded {} points to worker", new_points_count);
        }

        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Binning Interval:");
                let slider_res = ui.add(
                    egui::Slider::new(&mut self.step_interval, 1000..=100000).logarithmic(true),
                );

                if slider_res.drag_stopped() {
                    eprintln!("[UI] Slider released: requesting reset");
                    self.is_processing.store(true, Ordering::SeqCst);
                    let _ = self
                        .worker_tx
                        .send(ControlMsg::ResetInterval(self.step_interval));
                }

                if self.is_processing.load(Ordering::SeqCst) {
                    println!("[UI] Adding Spinner");
                    ui.add(egui::Spinner::new());
                    ui.weak("Processing...");
                    return;
                }

                if self.maximized_tag.is_some() && ui.button("⬅ Back").clicked() {
                    self.maximized_tag = None;
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(tag) = self.maximized_tag.clone() {
                if ui.button("📷").on_hover_text("Export PNG").clicked() {
                    let path = std::env::current_dir()
                        .unwrap()
                        .join(format!("{}.png", tag.replace("/", "_")));
                    screenshot_path = Some(path);
                }
                Plot::new("full")
                    .legend(Legend::default())
                    .default_x_bounds(0.0, 10e6)
                    .show(ui, |plot_ui| self.draw_plot_contents(plot_ui, &tag));
            } else {
                let cache = self.bin_cache.lock().unwrap();
                let mut sections: BTreeMap<String, Vec<String>> = BTreeMap::new();
                for tag in cache.keys() {
                    let parts: Vec<&str> = tag.split('/').collect();
                    let section = if parts.len() > 1 {
                        parts[..parts.len() - 1].join("/")
                    } else {
                        "General".into()
                    };
                    sections.entry(section).or_default().push(tag.clone());
                }
                drop(cache);

                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.heading("RL-Board Desktop");
                    for (section_name, tags) in sections {
                        egui::CollapsingHeader::new(&section_name)
                            .default_open(true)
                            .show(ui, |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    for tag in tags {
                                        self.render_thumbnail_plot(ui, &tag);
                                    }
                                });
                            });
                        ui.add_space(15.0);
                    }
                });
            }
        });
        if let Some(path) = screenshot_path.take() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(UserData::default()));
        }

        ctx.input(|i| {
            for event in &i.raw.events {
                if let egui::Event::Screenshot { image, .. } = event {
                    let path = PathBuf::from("plot_export.png"); // Or use a stored path
                    save_screenshot(path, image);
                }
            }
        });
        ctx.request_repaint();
    }
}
fn save_screenshot(path: PathBuf, image: &Arc<egui::ColorImage>) {
    let size = image.size;
    let pixels = image.as_raw(); // [r, g, b, a]

    if let Some(buffer) = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(
        size[0] as u32,
        size[1] as u32,
        pixels.to_vec(),
    ) {
        match buffer.save(&path) {
            Ok(_) => eprintln!("Saved screenshot to: {:?}", path),
            Err(e) => eprintln!("Failed to save screenshot: {}", e),
        }
    }
}
fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "RL-Board Desktop",
        native_options,
        Box::new(|cc| {
            Ok(Box::new(RLApp::new(
                cc,
                "/home/prometheus/PythonSpace/Model-free-planner/runs/HERs".to_string(),
                2,
            )))
        }),
    )
}

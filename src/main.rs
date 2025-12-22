use eframe::egui;
use egui_plot::{Corner, Legend, Line, Plot, PlotPoints};
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use tfrecord::{EventIter, protobuf::event::What, protobuf::summary::value::Value::SimpleValue};

struct ScalarPoint {
    run_name: String,
    tag: String,
    step: f64,
    value: f64,
}
struct BinnedData {
    xs: Vec<f64>,
    means: Vec<f64>,
    lowers: Vec<f64>,
    uppers: Vec<f64>,
}

struct RLApp {
    // Map<TagName, Map<RunName, Vec<[x, y]>>>
    all_data: BTreeMap<String, BTreeMap<String, Vec<[f64; 2]>>>,
    bin_cache: BTreeMap<(String, String), BinnedData>,
    receiver: Receiver<ScalarPoint>,
    maximized_tag: Option<String>,
    step_interval: u32, // The configurable bin width
}
fn start_live_monitor(
    root_path: &PathBuf,
    tx: Sender<ScalarPoint>,
    offsets: Arc<Mutex<std::collections::HashMap<PathBuf, u64>>>,
) {
    let (event_tx, event_rx) = std::sync::mpsc::channel();

    let mut watcher = RecommendedWatcher::new(event_tx, Config::default()).expect("Watcher fail");
    watcher
        .watch(root_path, RecursiveMode::Recursive)
        .expect("Watch path fail");

    println!("Starting Live monitor on dir {}", root_path.display());
    thread::spawn(move || {
        for res in event_rx {
            match res {
                Ok(event) => {
                    // We only care about file modifications
                    if let EventKind::Modify(_) = event.kind {
                        for path in event.paths {
                            if path.to_string_lossy().contains("tfevents") {
                                // Lock the shared offsets map
                                let mut offsets_map = offsets.lock().unwrap();
                                process_new_events(&path, &mut *offsets_map, &tx);
                            }
                        }
                    }
                }
                Err(e) => println!("watch error: {:?}", e),
            }
        }
    });
}

fn process_new_events(
    path: &PathBuf,
    offsets: &mut std::collections::HashMap<PathBuf, u64>,
    tx: &Sender<ScalarPoint>,
) {
    // 1. Get the last known offset or start at 0 if it's a brand new file
    let last_offset = *offsets.get(path).unwrap_or(&0);

    if let Ok(mut file) = File::open(path) {
        let metadata = file.metadata().expect("Metadata error");
        let current_len = metadata.len();

        // Only read if the file has actually grown
        if current_len > last_offset {
            // 2. Jump to the end of the previous read
            let _ = file.seek(SeekFrom::Start(last_offset));

            // 3. Read only the new bytes
            // We use with_reader so we don't re-open the file handle unnecessarily
            let reader = EventIter::from_reader(file, Default::default());
            let run_name = path
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "root".to_string());

            for result in reader.flatten() {
                if let Some(What::Summary(summary)) = result.what {
                    for val in summary.value {
                        if let Some(SimpleValue(v)) = val.value {
                            let _ = tx.send(ScalarPoint {
                                run_name: run_name.clone(),
                                tag: val.tag,
                                step: result.step as f64,
                                value: v as f64,
                            });
                        }
                    }
                }
            }
        }
        // 4. Update the offset for the next modify event
        offsets.insert(path.clone(), current_len);
    }
}
/// Recursive function to find tfevent files up to a certain depth
fn visit_dirs(
    dir: &Path,
    tx: &Sender<ScalarPoint>,
    depth: usize,
    max_depth: usize,
    offsets: &Arc<Mutex<std::collections::HashMap<PathBuf, u64>>>,
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
            } else if path.is_file() {
                // Check if it's a tfevents file
                if path
                    .file_name()
                    .map_or(false, |s| s.to_string_lossy().contains("tfevents"))
                {
                    // Determine run name based on parent directory
                    let run_name = path
                        .parent()
                        .and_then(|p| p.file_name())
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "root".to_string());
                    let tx_file = tx.clone();
                    let offsets_file = Arc::clone(offsets);
                    let handle = thread::spawn(move || {
                        if let Ok(reader) = EventIter::open(&path, Default::default()) {
                            for result in reader.flatten() {
                                if let Some(What::Summary(summary)) = result.what {
                                    for val in summary.value {
                                        if let Some(SimpleValue(v)) = val.value {
                                            let _ = tx_file.send(ScalarPoint {
                                                run_name: run_name.clone(),
                                                tag: val.tag,
                                                step: result.step as f64,
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
}
impl RLApp {
    fn new(_cc: &eframe::CreationContext<'_>, root_path: String, max_depth: usize) -> Self {
        let (tx, rx) = channel();
        let root = PathBuf::from(root_path);
        let root_clone = root.clone();
        // Shared registry for file offsets
        let offsets = Arc::new(Mutex::new(std::collections::HashMap::<PathBuf, u64>::new()));
        // Start recursive scanning in a background thread
        let tx_clone = tx.clone();
        let tx_clone2 = tx.clone();
        let offsets_history = Arc::clone(&offsets);
        thread::spawn(move || {
            let mut handles = vec![];
            visit_dirs(
                &root,
                &tx_clone,
                0,
                max_depth,
                &offsets_history,
                &mut handles,
            );
            for h in handles {
                let _ = h.join();
            }
            start_live_monitor(&root_clone, tx_clone2, offsets_history);
        });
        println!("Finished init");
        // thread::spawn(move || );

        Self {
            all_data: BTreeMap::new(),
            bin_cache: BTreeMap::new(),
            receiver: rx,
            maximized_tag: None,
            step_interval: 10_000,
        }
    }

    fn render_thumbnail_plot(&mut self, ui: &mut egui::Ui, tag: &str) {
        let runs_map = &self.all_data[tag];
        let card_width = 450.0;

        ui.allocate_ui(egui::vec2(card_width, 280.0), |ui| {
            ui.group(|ui| {
                ui.vertical(|ui| {
                    // Header
                    ui.horizontal(|ui| {
                        ui.add(egui::Label::new(tag).truncate());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("⛶").clicked() {
                                self.maximized_tag = Some(tag.to_string());
                            }
                        });
                    });

                    ui.add_space(4.0);

                    // Plot
                    let plot = Plot::new(tag)
                        .view_aspect(2.0)
                        .width(ui.available_width())
                        .height(200.0)
                        .allow_drag(false)
                        .allow_zoom(false)
                        .legend(
                            Legend::default()
                                .position(Corner::RightBottom)
                                .text_style(egui::TextStyle::Small),
                        );

                    plot.show(ui, |plot_ui| {
                        for (run_name, points) in runs_map {
                            let stride = (points.len() / 5_000).max(1);
                            let plot_points =
                                PlotPoints::from_iter(points.iter().step_by(stride).map(|&p| p));
                            plot_ui.line(Line::new(run_name, plot_points).name(run_name));
                        }
                    });
                });
            });
        });
    }

    fn render_fullscreen(&mut self, ui: &mut egui::Ui, tag: &str) {
        ui.horizontal(|ui| {
            if ui.button("⬅ Back").clicked() {
                self.maximized_tag = None;
            }
            ui.heading(format!("Full View: {}", tag));
        });

        ui.separator();

        if let Some(runs_map) = self.all_data.get(tag) {
            let plot = Plot::new(format!("full_{}", tag))
                .width(ui.available_width())
                .height(ui.available_height() - 40.0)
                .legend(Legend::default().position(Corner::RightTop));

            plot.show(ui, |plot_ui| {
                for (run_name, points) in runs_map {
                    let stride = (points.len() / 50_000).max(1);
                    let plot_points =
                        PlotPoints::from_iter(points.iter().step_by(stride).map(|&p| p));
                    plot_ui.line(Line::new(run_name, plot_points).name(run_name).width(2.0));
                }
            });
        }
    }
}

impl eframe::App for RLApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drain receiver
        while let Ok(point) = self.receiver.try_recv() {
            self.all_data
                .entry(point.tag)
                .or_default()
                .entry(point.run_name)
                .or_default()
                .push([point.step, point.value]);
        }
        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Binning Step Interval:");
                // Slider for 1k to 100k steps
                let changed = ui
                    .add(
                        egui::Slider::new(&mut self.step_interval, 1000..=100000).logarithmic(true),
                    )
                    .changed();

                // If interval changes, clear cache so everything re-bins
                if changed {
                    self.bin_cache.clear();
                }

                if self.maximized_tag.is_some() {
                    if ui.button("⬅ Back to Grid").clicked() {
                        self.maximized_tag = None;
                    }
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let current_maximized = self.maximized_tag.clone();

            if let Some(tag) = current_maximized {
                self.render_fullscreen(ui, &tag);
            } else {
                ui.heading("RL-Board Desktop (Recursive)");

                let mut sections: BTreeMap<String, Vec<String>> = BTreeMap::new();
                for tag in self.all_data.keys() {
                    let parts: Vec<&str> = tag.split('/').collect();
                    let section = if parts.len() > 1 {
                        parts[..parts.len() - 1].join("/")
                    } else {
                        "General".into()
                    };
                    sections.entry(section).or_default().push(tag.clone());
                }

                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (section_name, tags) in sections {
                        egui::CollapsingHeader::new(&section_name)
                            .default_open(true)
                            .show(ui, |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
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
        ctx.request_repaint();
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

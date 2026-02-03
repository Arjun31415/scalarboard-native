use clap::Parser;
use dashmap::DashMap;
use eframe::egui::{self, UserData, Vec2b};
use egui_plot::{Corner, FilledArea, Legend, Line, Plot};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
pub mod data;
pub mod io;
use data::*;
use io::*;

#[derive(Parser, Debug)]
#[command(
    author = "Arjun31415",
    version = "0.0.1",
    about = "ScalarBoard: A high-performance TensorBoard-like viewer in Rust for scalars"
)]
struct Args {
    /// The root directory containing tfevents files
    #[arg(short, long, default_value = ".")]
    path: String,

    /// Maximum recursion depth for finding files
    #[arg(short, long, default_value_t = 2)]
    depth: usize,
}

fn start_compute_worker(
    receiver: Receiver<ControlMsg>,
    cache: SharedCache,
    is_processing: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let mut raw_store: BTreeMap<Arc<str>, BTreeMap<Arc<str>, Vec<(u32, f32)>>> =
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
                            .push((p.step, p.value));
                        pending_tags.insert(p.tag);
                    }
                    ControlMsg::ResetInterval(new_interval) => {
                        current_interval = new_interval;
                        reset_requested = true;
                    }
                }
            }

            if reset_requested {
                raw_store.par_iter().for_each(|(tag, runs)| {
                    // If is it is eval we dont do buckets anyways so why bother recomputing
                    if tag.starts_with("eval") {
                        return;
                    }
                    runs.par_iter().for_each(|(run_name, points)| {
                        let tag_entry = cache.entry(tag.clone()).or_default();
                        let run_map = tag_entry.value();
                        let mut entry = run_map
                            .entry(run_name.clone())
                            .or_insert_with(|| DataStore::Binned(BinnedData::default()));
                        entry.update(&points, current_interval, true);
                    });
                });
                is_processing.store(false, Ordering::SeqCst);
            } else {
                pending_tags.par_drain().for_each(|tag| {
                    let is_eval = tag.starts_with("eval");
                    if let Some(runs) = raw_store.get(&tag) {
                        runs.par_iter().for_each(|(run_name, points)| {
                            let tag_entry = cache.entry(tag.clone()).or_default();
                            let run_map = tag_entry.value();
                            let mut entry = run_map.entry(run_name.clone()).or_insert_with(|| {
                                if is_eval {
                                    DataStore::Raw(RawPoints::default())
                                } else {
                                    DataStore::Binned(BinnedData::default())
                                }
                            });
                            entry.update(points, current_interval, false);
                        })
                    }
                })
            }
        }
    });
}

// --- UI APPLICATION ---

struct RLApp {
    bin_cache: SharedCache,
    worker_tx: Sender<ControlMsg>,
    raw_receiver: Receiver<ScalarPoint>,
    maximized_tag: Option<Arc<str>>,
    step_interval: u32,
    is_processing: Arc<AtomicBool>,
}
// Gemini generated because I had no idea how to do this, still not good enough, but I'm lazy
fn get_color_for_run(run_name: &str, is_dark: bool) -> egui::Color32 {
    // 1. Generate a stable seed from the string
    let mut h: u64 = 0x811c9dc5; // FNV offset basis
    for byte in run_name.bytes() {
        h ^= byte as u64;
        h = h.wrapping_mul(0x100000001b3); // FNV prime
    }

    // 2. The "Mixer": SplitMix64 or MurmurHash3 style. Idk if this claim is true but whatever
    // It shuffles the bits so the distribution is "flat" across the 0..1 range.
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d049bb133111eb);
    h = h ^ (h >> 31);

    // 3. Convert to Hue (0.0 to 1.0)
    let golden_ratio = 0.618033988749895;
    let hue = ((h as f64 / u64::MAX as f64) + golden_ratio).fract() as f32;
    // 4. Theme-aware Saturation and Value, only line that I understand
    let (s, v) = if is_dark { (0.7, 0.95) } else { (0.85, 0.65) };

    egui::Color32::from(egui::ecolor::Hsva::new(hue, s, v, 1.0))
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
            visit_dirs(&root, &tx_scan, 0, max_depth, &offsets_scan);
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
        let is_dark = plot_ui.ctx().style().visuals.dark_mode;
        if let Some(runs) = self.bin_cache.get(tag) {
            for entry in runs.iter() {
                let run_name = entry.key();
                let base_color = get_color_for_run(run_name, is_dark);

                match entry.value() {
                    DataStore::Binned(binned) => {
                        if binned.lowers.is_empty() {
                            continue;
                        }
                        // Draw Area (for std deviation, lower + upper) and Line
                        plot_ui.add(
                            FilledArea::new(
                                format!("{}_area", run_name),
                                &binned.x_coords,
                                &binned.lowers,
                                &binned.uppers,
                            )
                            .fill_color(base_color.linear_multiply(0.15)),
                        );
                        plot_ui.line(
                            Line::new(run_name.to_string(), binned.line_coords.clone())
                                .color(base_color)
                                .width(2.0),
                        );
                    }
                    DataStore::Raw(raw) => {
                        if raw.coords.is_empty() {
                            continue;
                        }
                        plot_ui.line(
                            Line::new(run_name.to_string(), raw.coords.clone())
                                .color(base_color)
                                .style(egui_plot::LineStyle::Dashed { length: 4.0 })
                                .name(format!("{} (eval)", run_name)),
                        );
                    }
                }
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
                egui::widgets::global_theme_preference_buttons(ui);
                ui.separator();
                ui.label("Binning Interval:");
                let slider_res = ui.add(
                    egui::Slider::new(&mut self.step_interval, 300..=100000).logarithmic(true),
                );
                if slider_res.changed() || slider_res.dragged() {
                    should_repaint = true;
                }
                if slider_res.changed() {
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
                    .x_axis_label(egui::RichText::new("Step").size(20.0).strong())
                    .y_axis_label(
                        egui::RichText::new(
                            tag.as_ref()
                                .split("/")
                                .last()
                                .or(Some(tag.as_ref()))
                                .unwrap(),
                        )
                        .size(20.0)
                        .strong(),
                    )
                    .show(ui, |plot_ui| self.draw_plot_contents(plot_ui, tag.as_ref()));
            } else {
                let mut sections: BTreeMap<String, Vec<Arc<str>>> = BTreeMap::new();
                for r in self.bin_cache.iter() {
                    let tag = r.key();
                    let section = tag.split('/').next().unwrap_or("General").to_string();
                    sections.entry(section).or_default().push(tag.clone());
                }

                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.heading("ScalarBoard Desktop");
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
            // FIXME:
            // there is still some bug that the plots dont get auto updated if there is new data, I
            // have to jiggle my mouse, idk what I have done wrong.
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

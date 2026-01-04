use crate::data::ScalarPoint;
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rayon::prelude::*;

use std::collections::HashMap;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::{Sender, channel};
use std::thread;
use tfrecord::{EventIter, protobuf::event::What, protobuf::summary::value::Value::SimpleValue};
pub fn process_new_events(
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

pub fn start_live_monitor(
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
                        if path.file_name().map_or(false, |name| {
                            name.to_string_lossy().starts_with("events.out.tfevents")
                        }) {
                            let mut offsets_map = offsets.lock().unwrap();
                            process_new_events(&path, &mut *offsets_map, &tx);
                        }
                    }
                }
            }
        }
    });
}

pub fn visit_dirs(
    dir: &Path,
    tx: &Sender<ScalarPoint>,
    depth: usize,
    max_depth: usize,
    offsets: &Arc<Mutex<HashMap<PathBuf, u64>>>,
) {
    if depth > max_depth {
        return;
    }

    if let Ok(entries) = std::fs::read_dir(dir) {
        // Collect into a Vec so we can use par_iter
        let entries: Vec<_> = entries.flatten().collect();

        entries.into_par_iter().for_each(|entry| {
            let path = entry.path();
            if path.is_dir() {
                // Recursive call within the pool
                visit_dirs(&path, tx, depth + 1, max_depth, offsets);
            } else if path.is_file()
                && path.file_name().map_or(false, |name| {
                    name.to_string_lossy().starts_with("events.out.tfevents")
                })
            {
                process_single_file(&path, tx, offsets);
            }
        });
    }
}

pub fn process_single_file(
    path: &Path,
    tx: &Sender<ScalarPoint>,
    offsets: &Arc<Mutex<HashMap<PathBuf, u64>>>,
) {
    if let Ok(reader) = EventIter::open(path, Default::default()) {
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
        if let Ok(meta) = std::fs::metadata(path) {
            offsets
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), meta.len());
        }
    }
}

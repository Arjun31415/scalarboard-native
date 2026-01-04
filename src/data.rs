use dashmap::DashMap;
use std::sync::Arc;
pub struct ScalarPoint {
    pub run_name: Arc<str>,
    pub tag: Arc<str>,
    pub step: u32,
    pub value: f32,
}
#[derive(Default, Debug, Clone)]
pub struct RawPoints {
    pub coords: Vec<[f64; 2]>,
    pub processed_upto: usize,
}
#[derive(Debug)]
pub enum DataStore {
    Binned(BinnedData),
    // Raw points used for eval tags
    Raw(RawPoints),
}
#[derive(Default, Debug, Clone)]
pub struct BinnedData {
    pub x_coords: Vec<f64>,
    pub line_coords: Vec<[f64; 2]>, // [x, y] for Line
    pub lowers: Vec<f64>,
    pub uppers: Vec<f64>,

    // Computation Stuff
    m2: Vec<f32>,
    num_elements: Vec<u32>,
    processed_upto: usize,
}
pub enum ControlMsg {
    NewPoint(ScalarPoint),
    ResetInterval(u32),
}

pub type SharedCache = Arc<DashMap<Arc<str>, DashMap<Arc<str>, DataStore>>>;

impl DataStore {
    pub fn update(&mut self, points: &[(u32, f32)], interval: u32, force_reset: bool) {
        match self {
            DataStore::Raw(raw) => {
                let start = if force_reset { 0 } else { raw.processed_upto };
                if force_reset {
                    raw.coords.clear();
                }

                for i in start..points.len() {
                    let (step, val) = points[i];
                    raw.coords.push([step as f64, val as f64]);
                }
                raw.processed_upto = points.len();
            }
            DataStore::Binned(binned) => {
                if force_reset {
                    *binned = BinnedData::default();
                }
                binned.update_incrementally(points, interval);
            }
        }
    }
}

impl BinnedData {
    pub fn update_incrementally(&mut self, raw_data: &[(u32, f32)], bin_width: u32) {
        if raw_data.is_empty() || self.processed_upto >= raw_data.len() {
            return;
        }

        let last_step = raw_data.last().unwrap().0;
        let required_bins = ((last_step / bin_width) + 1) as usize;

        if self.lowers.len() < required_bins {
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
            let bin_idx = (p.0 / bin_width) as usize;

            self.num_elements[bin_idx] += 1;
            let n = self.num_elements[bin_idx] as f32;
            let mut mean = self.line_coords[bin_idx][1] as f32;

            let delta = p.1 - mean;
            mean += delta / n;
            let delta2 = p.1 - mean;
            self.m2[bin_idx] += delta * delta2;

            let std_dev = (self.m2[bin_idx] / n).sqrt();
            self.uppers[bin_idx] = (mean + std_dev).into();
            self.lowers[bin_idx] = (mean - std_dev).into();
            self.line_coords[bin_idx] = [self.x_coords[bin_idx], mean as f64];
        }

        // If a bin is empty, use the value from the previous bin
        // (linear interpolation will be better but I'm lazy and this is good enough)
        for i in 1..required_bins {
            if self.num_elements[i] == 0 {
                self.lowers[i] = self.lowers[i - 1];
                self.uppers[i] = self.uppers[i - 1];
                self.line_coords[i] = [self.x_coords[i], self.line_coords[i - 1][1]];
            }
        }

        self.processed_upto = raw_data.len();
    }
}

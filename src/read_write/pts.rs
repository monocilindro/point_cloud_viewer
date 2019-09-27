// Copyright 2016 Google Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::{AttributeData, NumberOfPoints, PointsBatch};
use cgmath::Vector3;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

#[derive(Debug)]
pub struct PtsIterator {
    data: BufReader<File>,
    batch_size: usize,
}

impl PtsIterator {
    pub fn from_file(filename: &Path, batch_size: usize) -> Self {
        let file = File::open(filename).unwrap();
        PtsIterator {
            data: BufReader::new(file),
            batch_size,
        }
    }
}

impl NumberOfPoints for PtsIterator {
    fn num_points(&self) -> Option<usize> {
        None
    }
}
impl Iterator for PtsIterator {
    type Item = PointsBatch;

    fn next(&mut self) -> Option<PointsBatch> {
        let mut batch = PointsBatch {
            position: Vec::with_capacity(self.batch_size),
            attributes: [(
                "color".to_string(),
                AttributeData::U8Vec3(Vec::with_capacity(self.batch_size)),
            )]
            .iter()
            .cloned()
            .collect(),
        };
        let mut point_count = 0;
        let mut line = String::new();
        while point_count < self.batch_size {
            line.clear();
            self.data.read_line(&mut line).unwrap();
            if line.is_empty() {
                break;
            }

            let parts: Vec<&str> = line.trim().split(|c| c == ' ' || c == ',').collect();
            if parts.len() != 7 {
                continue;
            }
            batch.position.push(Vector3::new(
                parts[0].parse::<f64>().unwrap(),
                parts[1].parse::<f64>().unwrap(),
                parts[2].parse::<f64>().unwrap(),
            ));
            batch
                .get_attribute_vec_mut("color")
                .unwrap()
                .push(Vector3::new(
                    parts[4].parse::<u8>().unwrap(),
                    parts[5].parse::<u8>().unwrap(),
                    parts[6].parse::<u8>().unwrap(),
                ));
            point_count += 1;
        }
        if point_count == 0 {
            None
        } else {
            Some(batch)
        }
    }
}

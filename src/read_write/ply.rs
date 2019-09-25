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

use crate::errors::*;
use crate::read_write::{
    DataWriter, Encoding, NodeWriter, OpenMode, PositionEncoding, WriteEncoded, WriteLE, WriteLEPos,
};
use crate::{AttributeData, Point, PointsBatch};
use byteorder::{ByteOrder, LittleEndian};
use cgmath::Vector3;
use num_integer::div_ceil;
use num_traits::identities::Zero;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::ops::Index;
use std::path::{Path, PathBuf};
use std::str::{from_utf8, FromStr};

const HEADER_START_TO_NUM_VERTICES: &[u8] =
    b"ply\nformat binary_little_endian 1.0\nelement vertex ";
const HEADER_NUM_VERTICES: &[u8] = b"00000000000000000000";

#[derive(Debug)]
struct Header {
    format: Format,
    elements: Vec<Element>,
    offset: Vector3<f64>,
}

#[derive(Debug, Copy, Clone, PartialEq)]
enum DataType {
    Int8,
    Uint8,
    Int16,
    Uint16,
    Int32,
    Uint32,
    Int64,  // Not a legal datatype
    Uint64, // Not a legal datatype
    Float32,
    Float64,
}

impl DataType {
    fn from_str(input: &str) -> Result<Self> {
        match input {
            "float" | "float32" => Ok(DataType::Float32),
            "double" | "float64" => Ok(DataType::Float64),
            "char" | "int8" => Ok(DataType::Int8),
            "uchar" | "uint8" => Ok(DataType::Uint8),
            "short" | "int16" => Ok(DataType::Int16),
            "ushort" | "uint16" => Ok(DataType::Uint16),
            "int" | "int32" => Ok(DataType::Int32),
            "uint" | "uint32" => Ok(DataType::Uint32),
            "longlong" | "int64" => Ok(DataType::Int64),
            "ulonglong" | "uint64" => Ok(DataType::Uint64),
            _ => Err(ErrorKind::InvalidInput(format!("Invalid data type: {}", input)).into()),
        }
    }
}

impl Header {
    fn has_element(&self, name: &str) -> bool {
        self.elements.iter().any(|e| e.name == name)
    }
}

impl<'a> Index<&'a str> for Header {
    type Output = Element;
    fn index(&self, name: &'a str) -> &Self::Output {
        for element in &self.elements {
            if element.name == name {
                return element;
            }
        }
        panic!("Element {} does not exist.", name);
    }
}

#[derive(Debug, PartialEq)]
enum Format {
    BinaryLittleEndianV1,
    BinaryBigEndianV1,
    AsciiV1,
}

// TODO(hrapp): Maybe support list properties too?
#[derive(Debug, Clone)]
struct ScalarProperty {
    name: String,
    data_type: DataType,
}

#[derive(Debug)]
struct Element {
    name: String,
    count: i64,
    properties: Vec<ScalarProperty>,
}

impl<'a> Index<&'a str> for Element {
    type Output = ScalarProperty;
    fn index(&self, name: &'a str) -> &Self::Output {
        for p in &self.properties {
            if p.name == name {
                return p;
            }
        }
        panic!("Property does not exist!")
    }
}

fn parse_header<R: BufRead>(reader: &mut R) -> Result<(Header, usize)> {
    use crate::errors::ErrorKind::InvalidInput;

    let mut header_len = 0;
    let mut line = String::new();
    header_len += reader.read_line(&mut line)?;
    if line.trim() != "ply" {
        return Err(InvalidInput("Not a PLY file".to_string()).into());
    }

    let mut format = None;
    let mut current_element = None;
    let mut offset = Vector3::zero();
    let mut elements = Vec::new();
    loop {
        line.clear();
        header_len += reader.read_line(&mut line)?;
        let entries: Vec<&str> = line.trim().split_whitespace().collect();
        match entries[0] {
            "format" if entries.len() == 3 => {
                if entries[2] != "1.0" {
                    return Err(InvalidInput(format!("Invalid version: {}", entries[2])).into());
                }
                format = Some(match entries[1] {
                    "ascii" => Format::AsciiV1,
                    "binary_little_endian" => Format::BinaryLittleEndianV1,
                    "binary_big_endian" => Format::BinaryBigEndianV1,
                    _ => return Err(InvalidInput(format!("Invalid format: {}", entries[1])).into()),
                });
            }
            "element" if entries.len() == 3 => {
                if let Some(element) = current_element.take() {
                    elements.push(element);
                }
                current_element = Some(Element {
                    name: entries[1].to_string(),
                    count: entries[2]
                        .parse::<i64>()
                        .chain_err(|| InvalidInput(format!("Invalid count: {}", entries[2])))?,
                    properties: Vec::new(),
                });
            }
            "property" => {
                if current_element.is_none() {
                    return Err(
                        InvalidInput(format!("property outside of element: {}", line)).into(),
                    );
                };
                let property = match entries[1] {
                    "list" if entries.len() == 5 => {
                        // We do not support list properties.
                        continue;
                    }
                    data_type_str if entries.len() == 3 => {
                        let data_type = DataType::from_str(data_type_str)?;
                        ScalarProperty {
                            name: entries[2].to_string(),
                            data_type,
                        }
                    }
                    _ => return Err(InvalidInput(format!("Invalid line: {}", line)).into()),
                };
                current_element.as_mut().unwrap().properties.push(property);
            }
            "end_header" => break,
            "comment" => {
                if entries.len() == 5 && entries[1] == "offset:" {
                    let x = entries[2]
                        .parse::<f64>()
                        .chain_err(|| InvalidInput(format!("Invalid offset: {}", entries[2])))?;
                    let y = entries[3]
                        .parse::<f64>()
                        .chain_err(|| InvalidInput(format!("Invalid offset: {}", entries[3])))?;
                    let z = entries[4]
                        .parse::<f64>()
                        .chain_err(|| InvalidInput(format!("Invalid offset: {}", entries[4])))?;
                    offset = Vector3::new(x, y, z)
                }
            }
            _ => return Err(InvalidInput(format!("Invalid line: {}", line)).into()),
        }
    }

    if let Some(element) = current_element {
        elements.push(element);
    }

    if format.is_none() {
        return Err(InvalidInput("No format specified".into()).into());
    }

    Ok((
        Header {
            elements,
            format: format.unwrap(),
            offset,
        },
        header_len,
    ))
}

type ReadingFn = fn(nread: &mut usize, buf: &[u8], val: &mut PointsBatch, name: &str);

// The two macros create a 'ReadingFn' that reads a value of '$data_type' out of a reader, and
// calls '$assign' with it while casting it to the correct type. I did not find a way of doing this
// purely using generic programming, so I resorted to this macro.
macro_rules! create_and_return_reading_fn {
    ($assign:expr, $size:ident, $num_bytes:expr, $reading_fn:expr) => {{
        $size += $num_bytes;
        |nread: &mut usize, buf: &[u8], batch: &mut PointsBatch, name: &str| {
            #[allow(clippy::cast_lossless)]
            $assign(batch, name, $reading_fn(buf) as _);
            *nread += $num_bytes;
        }
    }};
}

macro_rules! read_casted_property {
    ($prop:expr, $assign:expr, &mut $size:ident) => {
        PropertyReader {
            prop: $prop.clone(),
            func: match $prop.data_type {
                DataType::Uint8 => {
                    create_and_return_reading_fn!($assign, $size, 1, |buf: &[u8]| buf[0])
                }
                DataType::Int8 => {
                    create_and_return_reading_fn!($assign, $size, 1, |buf: &[u8]| buf[0])
                }
                DataType::Uint16 => {
                    create_and_return_reading_fn!($assign, $size, 2, LittleEndian::read_u16)
                }
                DataType::Int16 => {
                    create_and_return_reading_fn!($assign, $size, 2, LittleEndian::read_i16)
                }
                DataType::Uint32 => {
                    create_and_return_reading_fn!($assign, $size, 4, LittleEndian::read_u32)
                }
                DataType::Int32 => {
                    create_and_return_reading_fn!($assign, $size, 4, LittleEndian::read_i32)
                }
                DataType::Uint64 => {
                    create_and_return_reading_fn!($assign, $size, 4, LittleEndian::read_u64)
                }
                DataType::Int64 => {
                    create_and_return_reading_fn!($assign, $size, 4, LittleEndian::read_i64)
                }
                DataType::Float32 => {
                    create_and_return_reading_fn!($assign, $size, 4, LittleEndian::read_f32)
                }
                DataType::Float64 => {
                    create_and_return_reading_fn!($assign, $size, 8, LittleEndian::read_f64)
                }
            },
        }
    };
}

macro_rules! push_reader {
    ($readers:ident, $prop:expr, &mut $num_bytes:ident, $dtype:ty) => {{
        $readers.push(read_casted_property!(
            $prop,
            |p: &mut PointsBatch, name: &str, val| {
                let vec: &mut Vec<$dtype> = p.get_attribute_vec_mut(name).unwrap();
                vec.push(val)
            },
            &mut $num_bytes
        ));
    }};
}

// Similar to 'create_and_return_reading_fn', but creates a function that just advances the read
// pointer.
macro_rules! create_skip_fn {
    ($prop:expr, &mut $size:ident, $num_bytes:expr) => {{
        println!("Will ignore property '{}' on 'vertex'.", $prop.name);
        $size += $num_bytes;
        fn _read_fn(nread: &mut usize, _: &[u8], _: &mut PointsBatch, _: &str) {
            *nread += $num_bytes;
        }
        PropertyReader {
            prop: $prop.clone(),
            func: _read_fn,
        }
    }};
}

struct PropertyReader {
    prop: ScalarProperty,
    func: ReadingFn,
}

/// Abstraction to read binary points from ply files into points.
pub struct PlyIterator {
    reader: BufReader<File>,
    readers: Vec<PropertyReader>,
    pub num_total_points: i64,
    batch_size: usize,
    offset: Vector3<f64>,
    point_count: usize,
}

impl PlyIterator {
    pub fn from_file<P: AsRef<Path>>(ply_file: P, batch_size: usize) -> Result<Self> {
        let mut file = File::open(ply_file).chain_err(|| "Could not open input file.")?;
        let mut reader = BufReader::new(file);
        let (header, header_len) = parse_header(&mut reader)?;
        file = reader.into_inner();
        file.seek(SeekFrom::Start(header_len as u64))?;

        if !header.has_element("vertex") {
            panic!("Header does not have element 'vertex'");
        }

        if header.format != Format::BinaryLittleEndianV1 {
            panic!("Unsupported PLY format: {:?}", header.format);
        }

        let vertex = &header["vertex"];
        let mut seen_x = false;
        let mut seen_y = false;
        let mut seen_z = false;

        let mut readers: Vec<PropertyReader> = Vec::new();
        let mut num_bytes_per_point = 0;

        for prop in &vertex.properties {
            match &prop.name as &str {
                "x" => {
                    readers.push(read_casted_property!(
                        prop,
                        |p: &mut PointsBatch, _: &str, val: f64| p.position.last_mut().unwrap().x =
                            val,
                        &mut num_bytes_per_point
                    ));
                    seen_x = true;
                }
                "y" => {
                    readers.push(read_casted_property!(
                        prop,
                        |p: &mut PointsBatch, _: &str, val: f64| p.position.last_mut().unwrap().y =
                            val,
                        &mut num_bytes_per_point
                    ));
                    seen_y = true;
                }
                "z" => {
                    readers.push(read_casted_property!(
                        prop,
                        |p: &mut PointsBatch, _: &str, val: f64| p.position.last_mut().unwrap().z =
                            val,
                        &mut num_bytes_per_point
                    ));
                    seen_z = true;
                }
                "r" | "red" => {
                    readers.push(read_casted_property!(
                        prop,
                        |p: &mut PointsBatch, _: &str, val: u8| {
                            let vec: &mut Vec<Vector3<u8>> =
                                p.get_attribute_vec_mut("color").unwrap();
                            vec.last_mut().unwrap().x = val
                        },
                        &mut num_bytes_per_point
                    ));
                }
                "g" | "green" => {
                    readers.push(read_casted_property!(
                        prop,
                        |p: &mut PointsBatch, _: &str, val: u8| {
                            let vec: &mut Vec<Vector3<u8>> =
                                p.get_attribute_vec_mut("color").unwrap();
                            vec.last_mut().unwrap().y = val
                        },
                        &mut num_bytes_per_point
                    ));
                }
                "b" | "blue" => {
                    readers.push(read_casted_property!(
                        prop,
                        |p: &mut PointsBatch, _: &str, val: u8| {
                            let vec: &mut Vec<Vector3<u8>> =
                                p.get_attribute_vec_mut("color").unwrap();
                            vec.last_mut().unwrap().z = val
                        },
                        &mut num_bytes_per_point
                    ));
                }
                "a" | "alpha" => {
                    readers.push(create_skip_fn!(prop, &mut num_bytes_per_point, 1));
                }
                _ => {
                    use self::DataType::*;
                    match prop.data_type {
                        Uint8 => push_reader!(readers, prop, &mut num_bytes_per_point, u8),
                        Uint64 => push_reader!(readers, prop, &mut num_bytes_per_point, u64),
                        Int64 => push_reader!(readers, prop, &mut num_bytes_per_point, i64),
                        Float32 => push_reader!(readers, prop, &mut num_bytes_per_point, f32),
                        Float64 => push_reader!(readers, prop, &mut num_bytes_per_point, f64),
                        Int8 => readers.push(create_skip_fn!(prop, &mut num_bytes_per_point, 1)),
                        Uint16 | Int16 => {
                            readers.push(create_skip_fn!(prop, &mut num_bytes_per_point, 2))
                        }

                        Uint32 | Int32 => {
                            readers.push(create_skip_fn!(prop, &mut num_bytes_per_point, 4))
                        }
                    }
                }
            }
        }

        if !seen_x || !seen_y || !seen_z {
            panic!("PLY must contain properties 'x', 'y', 'z' for 'vertex'.");
        }

        // We align the buffer of this 'BufReader' to points, so that we can index this buffer and know
        // that it will always contain full points to parse.
        Ok(PlyIterator {
            reader: BufReader::with_capacity(num_bytes_per_point * 1024, file),
            readers,
            num_total_points: header["vertex"].count,
            batch_size,
            offset: header.offset,
            point_count: 0,
        })
    }
}

fn batch_from_readers(readers: &[PropertyReader]) -> PointsBatch {
    let mut batch = PointsBatch {
        position: Vec::new(),
        attributes: BTreeMap::new(),
    };
    for reader in readers {
        match &reader.prop.name as &str {
            "x" | "y" | "z" => {}
            "r" | "red" => {
                batch
                    .attributes
                    .insert("color".to_string(), AttributeData::U8Vec3(Vec::new()));
            }
            "g" | "green" | "b" | "blue" | "a" | "alpha" => {}
            other => {
                let name = if other.chars().last().unwrap().is_ascii_digit() {
                    // Multidimensional attributes other than position and color are currently unsupported.
                    unimplemented!();
                } else {
                    other
                };
                let data = match reader.prop.data_type {
                    DataType::Uint8 => AttributeData::U8(Vec::new()),
                    DataType::Uint64 => AttributeData::U64(Vec::new()),
                    DataType::Int64 => AttributeData::I64(Vec::new()),
                    DataType::Float32 => AttributeData::F32(Vec::new()),
                    DataType::Float64 => AttributeData::F64(Vec::new()),
                    DataType::Int8
                    | DataType::Uint16
                    | DataType::Int16
                    | DataType::Uint32
                    | DataType::Int32 => continue,
                };
                batch.attributes.insert(name.to_string(), data);
            }
        }
    }
    batch
}

/// Since we can't be sure in which order multidimensional attributes are read,
/// we push default values and then fill them inside the readers.
fn push_into_multidimensional_attributes(batch: &mut PointsBatch) {
    batch.position.push(Vector3::zero());
    for a in batch.attributes.values_mut() {
        match a {
            AttributeData::U8(_)
            | AttributeData::U64(_)
            | AttributeData::I64(_)
            | AttributeData::F32(_)
            | AttributeData::F64(_) => (),
            AttributeData::F64Vec3(data) => data.push(Vector3::zero()),
            AttributeData::U8Vec3(data) => data.push(Vector3::zero()),
        }
    }
}

impl Iterator for PlyIterator {
    type Item = PointsBatch;

    fn size_hint(&self) -> (usize, Option<usize>) {
        let num_batches = div_ceil(self.num_total_points as usize, self.batch_size);
        (num_batches, Some(num_batches))
    }

    fn next(&mut self) -> Option<PointsBatch> {
        if self.point_count == self.num_total_points as usize {
            return None;
        }

        let mut batch = batch_from_readers(&self.readers);

        while self.point_count < self.num_total_points as usize
            && batch.position.len() < self.batch_size
        {
            push_into_multidimensional_attributes(&mut batch);

            let mut nread = 0;

            // We made sure before that the internal buffer of 'reader' is aligned to the number of
            // bytes for a single point, therefore we can access it here and know that we can always
            // read into it and are sure that it contains at least a full point.
            {
                let buf = self.reader.fill_buf().unwrap();
                for r in &self.readers {
                    let cnread = nread;
                    (r.func)(&mut nread, &buf[cnread..], &mut batch, &r.prop.name);
                }
            }
            *batch.position.last_mut().unwrap() += self.offset;
            self.reader.consume(nread);
            self.point_count += 1;
        }

        Some(batch)
    }
}

pub struct PlyNodeWriter {
    writer: DataWriter,
    point_count: usize,
    encoding: Encoding,
}

impl NodeWriter<PointsBatch> for PlyNodeWriter {
    fn new(filename: impl Into<PathBuf>, encoding: Encoding, open_mode: OpenMode) -> Self {
        Self::new(filename, encoding, open_mode)
    }

    fn write(&mut self, p: &PointsBatch) -> io::Result<()> {
        if p.position.is_empty() {
            return Ok(());
        }
        if self.point_count == 0 {
            self.create_header(
                &p.attributes
                    .iter()
                    .map(|(k, data)| {
                        (
                            &k[..],
                            match data {
                                AttributeData::U8(_) => "uchar",
                                AttributeData::I64(_) => "longlong",
                                AttributeData::U64(_) => "ulonglong",
                                AttributeData::F32(_) => "float",
                                AttributeData::F64(_) => "double",
                                AttributeData::U8Vec3(_) => "uchar",
                                AttributeData::F64Vec3(_) => "double",
                            },
                            data.dim(),
                        )
                    })
                    .collect::<Vec<_>>()[..],
            )?;
        }

        for (i, pos) in p.position.iter().enumerate() {
            pos.write_encoded(&self.encoding, &mut self.writer)?;
            for data in p.attributes.values() {
                data.write_le_pos(i, &mut self.writer)?;
            }
        }

        self.point_count += p.position.len();

        Ok(())
    }
}

impl NodeWriter<Point> for PlyNodeWriter {
    fn new(filename: impl Into<PathBuf>, encoding: Encoding, open_mode: OpenMode) -> Self {
        Self::new(filename, encoding, open_mode)
    }

    fn write(&mut self, p: &Point) -> io::Result<()> {
        if self.point_count == 0 {
            let mut attributes = vec![("color", "uchar", 3)];
            if p.intensity.is_some() {
                attributes.push(("intensity", "float", 1));
            }
            self.create_header(&attributes)?;
        }

        p.position.write_encoded(&self.encoding, &mut self.writer)?;
        p.color.write_le(&mut self.writer)?;
        if let Some(i) = p.intensity {
            i.write_le(&mut self.writer)?;
        }

        self.point_count += 1;

        Ok(())
    }
}

impl Drop for PlyNodeWriter {
    fn drop(&mut self) {
        if self.point_count == 0 {
            return;
        }
        self.writer.write_all(b"\n").unwrap();
        if self
            .writer
            .seek(SeekFrom::Start(HEADER_START_TO_NUM_VERTICES.len() as u64))
            .is_ok()
        {
            let _res = write!(
                &mut self.writer,
                "{:0width$}",
                self.point_count,
                width = HEADER_NUM_VERTICES.len()
            );
        }
    }
}

impl PlyNodeWriter {
    pub fn new(filename: impl Into<PathBuf>, encoding: Encoding, open_mode: OpenMode) -> Self {
        let filename = filename.into();
        let mut point_count = 0;
        if open_mode == OpenMode::Append {
            if let Ok(mut file) = File::open(&filename) {
                if file.metadata().unwrap().len()
                    >= HEADER_START_TO_NUM_VERTICES.len() as u64 + HEADER_NUM_VERTICES.len() as u64
                {
                    file.seek(SeekFrom::Start(HEADER_START_TO_NUM_VERTICES.len() as u64))
                        .unwrap();
                    let mut buf = vec![0; HEADER_NUM_VERTICES.len()];
                    file.read_exact(&mut buf).unwrap();
                    point_count = usize::from_str(from_utf8(&buf).unwrap()).unwrap();
                }
            }
        }
        let mut writer = DataWriter::new(filename, open_mode).unwrap();
        if point_count > 0 {
            // Our ply files always have a newline at the end.
            writer.seek(SeekFrom::End(-1)).unwrap();
        }
        Self {
            writer,
            point_count,
            encoding,
        }
    }

    fn create_header(&mut self, elements: &[(&str, &str, usize)]) -> io::Result<()> {
        self.writer.write_all(HEADER_START_TO_NUM_VERTICES)?;
        self.writer.write_all(HEADER_NUM_VERTICES)?;
        self.writer.write_all(b"\n")?;
        let pos_data_str = match &self.encoding {
            Encoding::Plain => "double",
            Encoding::ScaledToCube(_, _, pos_enc) => match pos_enc {
                PositionEncoding::Uint8 => "uchar",
                PositionEncoding::Uint16 => "ushort",
                PositionEncoding::Float32 => "float",
                PositionEncoding::Float64 => "double",
            },
        };
        for pos in &["x", "y", "z"] {
            let prop = &["property", " ", pos_data_str, " ", pos, "\n"].concat();
            self.writer.write_all(&prop.as_bytes())?;
        }
        for (name, data_str, num_properties) in elements {
            match &name[..] {
                "color" | "rgb" | "rgba" => {
                    let colors = ["red", "green", "blue", "alpha"];
                    for color in colors.iter().take(*num_properties) {
                        let prop = &["property", " ", data_str, " ", color, "\n"].concat();
                        self.writer.write_all(&prop.as_bytes())?;
                    }
                }
                _ if *num_properties > 1 => {
                    for i in 0..*num_properties {
                        let prop =
                            &["property", " ", data_str, " ", name, &i.to_string(), "\n"].concat();
                        self.writer.write_all(&prop.as_bytes())?;
                    }
                }
                _ => {
                    let prop = &["property", " ", data_str, " ", name, "\n"].concat();
                    self.writer.write_all(&prop.as_bytes())?;
                }
            }
        }
        self.writer.write_all(b"end_header\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read_write::PlyIterator;
    use tempdir::TempDir;

    const BATCH_SIZE: usize = 2;
    const NUM_BATCHES: usize = 4;
    const LAST_BATCH: usize = 3;

    fn batches_from_file<P: AsRef<Path>>(path: P) -> Vec<PointsBatch> {
        let iterator = PlyIterator::from_file(path, BATCH_SIZE).unwrap();
        let mut batches = Vec::new();
        iterator.for_each(|batch| {
            batches.push(batch);
        });
        batches
    }

    #[test]
    fn test_xyz_f32_rgb_u8_le() {
        let batches = batches_from_file("src/test_data/xyz_f32_rgb_u8_le.ply");
        assert_eq!(NUM_BATCHES, batches.len());
        assert_eq!(batches[0].position[0].x, 1.);
        assert_eq!(batches[LAST_BATCH].position.last().unwrap().x, 22.);
        let color_first: &Vec<Vector3<u8>> = batches[0].get_attribute_vec("color").unwrap();
        let color_last: &Vec<Vector3<u8>> = batches[LAST_BATCH].get_attribute_vec("color").unwrap();
        assert_eq!(color_first[0].x, 255);
        assert_eq!(color_last.last().unwrap().x, 234);
    }

    #[test]
    fn test_xyz_f32_rgba_u8_le() {
        let batches = batches_from_file("src/test_data/xyz_f32_rgba_u8_le.ply");
        assert_eq!(NUM_BATCHES, batches.len());
        assert_eq!(batches[0].position[0].x, 1.);
        assert_eq!(batches[LAST_BATCH].position.last().unwrap().x, 22.);
        let color_first: &Vec<Vector3<u8>> = batches[0].get_attribute_vec("color").unwrap();
        let color_last: &Vec<Vector3<u8>> = batches[LAST_BATCH].get_attribute_vec("color").unwrap();
        assert_eq!(color_first[0].x, 255);
        assert_eq!(color_last.last().unwrap().x, 227);
    }

    #[test]
    fn test_xyz_f32_rgb_u8_intensity_f32_le() {
        // All intensities in this file are NaN, but set.
        let batches = batches_from_file("src/test_data/xyz_f32_rgb_u8_intensity_f32.ply");
        assert_eq!(NUM_BATCHES, batches.len());
        assert_eq!(batches[0].position[0].x, 1.);
        assert_eq!(batches[LAST_BATCH].position.last().unwrap().x, 22.);
        let intensity_first: std::result::Result<&Vec<f32>, String> =
            batches[0].get_attribute_vec("intensity");
        let intensity_last: std::result::Result<&Vec<f32>, String> =
            batches[LAST_BATCH].get_attribute_vec("intensity");
        assert!(intensity_first.is_ok() && intensity_first.unwrap().len() == BATCH_SIZE);
        assert!(intensity_last.is_ok() && intensity_last.unwrap().len() == BATCH_SIZE);
        let color_first: &Vec<Vector3<u8>> = batches[0].get_attribute_vec("color").unwrap();
        let color_last: &Vec<Vector3<u8>> = batches[LAST_BATCH].get_attribute_vec("color").unwrap();
        assert_eq!(color_first[0].x, 255);
        assert_eq!(color_last.last().unwrap().x, 234);
    }

    #[test]
    fn test_ply_read_write() {
        let tmp_dir = TempDir::new("test_ply_read_write").unwrap();
        let file_path_test = tmp_dir.path().join("out.ply");
        let file_path_gt = "src/test_data/xyz_f32_rgb_u8_intensity_f32.ply";
        {
            let mut ply_writer =
                PlyNodeWriter::new(&file_path_test, Encoding::Plain, OpenMode::Truncate);
            PlyIterator::from_file(file_path_gt, BATCH_SIZE)
                .unwrap()
                .for_each(|p| {
                    ply_writer.write(&p).unwrap();
                });
        }
        // Now append to the file
        {
            let mut ply_writer =
                PlyNodeWriter::new(&file_path_test, Encoding::Plain, OpenMode::Append);
            PlyIterator::from_file(file_path_gt, BATCH_SIZE)
                .unwrap()
                .for_each(|p| {
                    ply_writer.write(&p).unwrap();
                });
        }
        PlyIterator::from_file(file_path_gt, BATCH_SIZE)
            .unwrap()
            .chain(PlyIterator::from_file(file_path_gt, BATCH_SIZE).unwrap())
            .zip(PlyIterator::from_file(&file_path_test, BATCH_SIZE).unwrap())
            .for_each(|(gt, test)| {
                assert_eq!(gt.position, test.position);
                let gt_color: &Vec<Vector3<u8>> = gt.get_attribute_vec("color").unwrap();
                let test_color: &Vec<Vector3<u8>> = test.get_attribute_vec("color").unwrap();
                assert_eq!(gt_color, test_color);
                // All intensities in this file are NaN, but set.
                let gt_intensity: &Vec<f32> = gt.get_attribute_vec("intensity").unwrap();
                let test_intensity: &Vec<f32> = test.get_attribute_vec("intensity").unwrap();
                assert_eq!(gt_intensity.len(), test_intensity.len());
                assert!(gt_intensity.iter().all(|i| i.is_nan()));
                assert!(test_intensity.iter().all(|i| i.is_nan()));
            });
    }
}

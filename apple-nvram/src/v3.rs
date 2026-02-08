use std::{
    borrow::Cow,
    fmt::{Display, Formatter},
    ops::ControlFlow,
};

use crate::{Error, VarType};

// https://github.com/apple-oss-distributions/xnu/blob/main/iokit/Kernel/IONVRAMV3Handler.cpp#L630

const VARIABLE_STORE_SIGNATURE: &[u8; 4] = b"3VVN";
const VARIABLE_STORE_VERSION: u8 = 0x1;
const VARIABLE_DATA: u16 = 0x55AA;

const PARTITION_SIZE: usize = 0x10000;
const STORE_HEADER_SIZE: usize = 24;
const VAR_HEADER_SIZE: usize = 36;
const VAR_ADDED: u8 = 0x7F;
const VAR_IN_DELETED_TRANSITION: u8 = 0xFE;
const VAR_DELETED: u8 = 0xFD;

const APPLE_COMMON_VARIABLE_GUID: &[u8; 16] = &[
    0x7C, 0x43, 0x61, 0x10, 0xAB, 0x2A, 0x4B, 0xBB, 0xA8, 0x80, 0xFE, 0x41, 0x99, 0x5C, 0x9F, 0x82,
];
const APPLE_SYSTEM_VARIABLE_GUID: &[u8; 16] = &[
    0x40, 0xA0, 0xDD, 0xD2, 0x77, 0xF8, 0x43, 0x92, 0xB4, 0xA3, 0x1E, 0x73, 0x04, 0x20, 0x65, 0x16,
];

#[derive(Debug, Default)]
enum Slot<T> {
    Valid(T),
    Invalid,
    #[default]
    Empty,
}

impl<T> Slot<T> {
    pub const fn as_ref(&self) -> Slot<&T> {
        match self {
            Slot::Valid(v) => Slot::Valid(v),
            Slot::Invalid => Slot::Invalid,
            Slot::Empty => Slot::Empty,
        }
    }

    pub fn as_mut(&mut self) -> Slot<&mut T> {
        match self {
            Slot::Valid(v) => Slot::Valid(v),
            Slot::Invalid => Slot::Invalid,
            Slot::Empty => Slot::Empty,
        }
    }

    pub fn unwrap(self) -> T {
        match self {
            Slot::Valid(v) => v,
            Slot::Invalid => panic!("called `Slot::unwrap()` on an `Invalid` value"),
            Slot::Empty => panic!("called `Slot::unwrap()` on an `Empty` value"),
        }
    }

    pub fn empty(&self) -> bool {
        match self {
            Slot::Empty => true,
            _ => false,
        }
    }
}

#[derive(Debug)]
pub struct Nvram<'a> {
    partitions: [Slot<Partition<'a>>; 16],
    partition_count: usize,
    active: usize,
}

impl<'a> Nvram<'a> {
    pub fn parse(nvr: &'a [u8]) -> crate::Result<Nvram<'a>> {
        let partition_count = nvr.len() / PARTITION_SIZE;
        let mut partitions: [Slot<Partition<'a>>; 16] = Default::default();
        let mut active = 0;
        let mut max_gen = 0;
        let mut valid_partitions = 0;

        for i in 0..partition_count {
            let offset = i * PARTITION_SIZE;
            if offset >= nvr.len() {
                break;
            }
            match Partition::parse(&nvr[offset..offset + PARTITION_SIZE]) {
                Ok(p) => {
                    let p_gen = p.generation();
                    if p_gen > max_gen {
                        active = i;
                        max_gen = p_gen;
                    }
                    partitions[i] = Slot::Valid(p);
                    valid_partitions += 1;
                }
                Err(V3Error::Empty) => {
                    partitions[i] = Slot::Empty;
                }
                Err(_) => {
                    partitions[i] = Slot::Invalid;
                }
            }
        }

        if valid_partitions == 0 {
            return Err(Error::ParseError);
        }

        Ok(Nvram {
            partitions,
            partition_count,
            active,
        })
    }

    fn partitions(&self) -> impl Iterator<Item = &Partition<'a>> {
        self.partitions
            .iter()
            .take(self.partition_count)
            .filter_map(|x| match x {
                Slot::Valid(p) => Some(p),
                Slot::Invalid => None,
                Slot::Empty => None,
            })
    }

    fn active_part(&self) -> &Partition<'a> {
        self.partitions[self.active].as_ref().unwrap()
    }

    #[cfg(test)]
    fn active_part_mut(&mut self) -> &mut Partition<'a> {
        self.partitions[self.active].as_mut().unwrap()
    }
}

impl<'a> crate::Nvram<'a> for Nvram<'a> {
    fn serialize(&self) -> crate::Result<Vec<u8>> {
        let mut v = Vec::with_capacity(self.partition_count * PARTITION_SIZE);
        for p in self.partitions() {
            p.serialize(&mut v);
        }
        Ok(v)
    }

    fn prepare_for_write(&mut self) {
        // nop
    }

    fn partitions(&self) -> Box<dyn Iterator<Item = &dyn crate::Partition<'a>> + '_> {
        Box::new(self.partitions().map(|p| p as &dyn crate::Partition<'a>))
    }

    fn active_part_mut(&mut self) -> &mut dyn crate::Partition<'a> {
        self.partitions[self.active].as_mut().unwrap()
    }

    fn apply(&mut self, w: &mut dyn crate::NvramWriter) -> crate::Result<()> {
        let ap = self.active_part();
        let offset;
        // there aren't really any sections in v3 but the store header still
        // specifies limits for maximum combined size of each kind of variable
        if ap.system_used() > ap.system_size() {
            return Err(Error::SectionTooBig);
        }
        if ap.common_used() > ap.common_size() {
            return Err(Error::SectionTooBig);
        }

        // if total size is too big, copy added variables to the next bank
        if ap.total_used() <= ap.usable_size() {
            offset = (self.active * PARTITION_SIZE) as u32;
        } else {
            let new_active = (self.active + 1) % self.partition_count;
            offset = (new_active * PARTITION_SIZE) as u32;
            if !self.partitions[new_active].empty() {
                w.erase_if_needed(offset, PARTITION_SIZE);
            }
            // must only clone 0x7F variables to the next partition
            self.partitions[new_active] = Slot::Valid(
                self.partitions[self.active]
                    .as_ref()
                    .unwrap()
                    .clone_active(),
            );
            self.active = new_active;
            // we could still have too many active variables
            if self.active_part().total_used() > PARTITION_SIZE {
                return Err(Error::SectionTooBig);
            }
        }

        let mut data = Vec::with_capacity(PARTITION_SIZE);
        self.active_part().serialize(&mut data);
        w.write_all(offset, &data)
            .map_err(|e| Error::ApplyError(e))?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Partition<'a> {
    pub header: StoreHeader<'a>,
    pub values: Vec<Variable<'a>>,
    empty_region_end: usize,
}

#[derive(Debug)]
enum V3Error {
    ParseError,
    Empty,
}

type Result<T> = std::result::Result<T, V3Error>;

impl<'a> Partition<'a> {
    fn parse(nvr: &'a [u8]) -> Result<Partition<'a>> {
        if let Ok(header) = StoreHeader::parse(&nvr[..STORE_HEADER_SIZE]) {
            let mut offset = STORE_HEADER_SIZE;
            let mut values = Vec::new();
            // one byte past the last 0xFF or the end of partition
            let mut empty_region_end = header.size();

            while offset + VAR_HEADER_SIZE < header.size() {
                let mut empty = true;
                for i in 0..VAR_HEADER_SIZE {
                    if nvr[offset + i] != 0 && nvr[offset + i] != 0xFF {
                        empty = false;
                        break;
                    }
                }
                if empty {
                    // check where exactly the "empty" space ends
                    // (it might not be at the end of partition)
                    offset += VAR_HEADER_SIZE;
                    while offset < header.size() {
                        if nvr[offset] != 0xFF {
                            empty_region_end = offset;
                            break;
                        }
                        offset += 1
                    }
                    break;
                }

                let Ok(v_header) = VarHeader::parse(&nvr[offset..]) else {
                    // if there's no valid header, just end here and return values parsed so far
                    // we also know there is no space for adding any new or updated variables
                    empty_region_end = offset;
                    break;
                };

                let k_begin = offset + VAR_HEADER_SIZE;
                let k_end = k_begin + v_header.name_size as usize;
                let key = &nvr[k_begin..k_end - 1];

                let v_begin = k_end;
                let v_end = v_begin + v_header.data_size as usize;
                let value = &nvr[v_begin..v_end];

                let crc = crc32fast::hash(value);
                if crc != v_header.crc {
                    return Err(V3Error::ParseError);
                }
                let v = Variable {
                    header: v_header,
                    key: Cow::Borrowed(key),
                    value: Cow::Borrowed(value),
                };

                offset += v.size();
                values.push(v);
            }

            Ok(Partition {
                header,
                values,
                empty_region_end,
            })
        } else {
            match nvr.iter().copied().try_for_each(|v| match v {
                0xFF => ControlFlow::Continue(()),
                _ => ControlFlow::Break(()),
            }) {
                ControlFlow::Continue(_) => Err(V3Error::Empty),
                ControlFlow::Break(_) => Err(V3Error::ParseError),
            }
        }
    }

    fn generation(&self) -> u32 {
        self.header.generation
    }

    fn entries<'b, 'c>(
        &'b mut self,
        key: &'c [u8],
        typ: VarType,
    ) -> impl Iterator<Item = &'b mut Variable<'a>>
    where
        'a: 'b,
        'c: 'b,
    {
        self.values
            .iter_mut()
            .filter(move |e| e.key == key && e.typ() == typ)
    }

    fn entries_added<'b, 'c>(
        &'b mut self,
        key: &'c [u8],
        typ: VarType,
    ) -> impl Iterator<Item = &'b mut Variable<'a>>
    where
        'a: 'b,
        'c: 'b,
    {
        self.entries(key, typ)
            .filter(|v| v.header.state == VAR_ADDED)
    }

    // total size of store header + all variables including the inactive duplicates
    fn total_used(&self) -> usize {
        STORE_HEADER_SIZE + self.values.iter().fold(0, |acc, v| acc + v.size())
    }

    // size of active system variables
    fn system_used(&self) -> usize {
        self.values
            .iter()
            .filter(|&v| v.header.state == VAR_ADDED && v.header.guid == APPLE_SYSTEM_VARIABLE_GUID)
            .fold(0, |acc, v| acc + v.size())
    }

    // size of active common variables
    fn common_used(&self) -> usize {
        self.values
            .iter()
            .filter(|&v| v.header.state == VAR_ADDED && v.header.guid == APPLE_COMMON_VARIABLE_GUID)
            .fold(0, |acc, v| acc + v.size())
    }

    fn system_size(&self) -> usize {
        self.header.system_size as usize
    }

    fn common_size(&self) -> usize {
        self.header.common_size as usize
    }

    // total usable size, usually equal to partition size
    // unless there are any non-0xFF bytes after last valid variable
    fn usable_size(&self) -> usize {
        self.empty_region_end
    }

    fn serialize(&self, v: &mut Vec<u8>) {
        let start_size = v.len();
        self.header.serialize(v);
        // Here we actually want to iterate over all versions of variables so we use the struct field directly.
        for var in &self.values {
            var.serialize(v);
        }
        let my_size = v.len() - start_size;
        debug_assert!(v.len() == self.total_used());

        // padding
        for _ in 0..(self.header.size() - my_size) {
            v.push(0xFF);
        }
    }

    fn variables(&self) -> impl Iterator<Item = &Variable<'a>> {
        self.values.iter().filter(|v| v.header.state == VAR_ADDED)
    }

    fn clone_active(&self) -> Partition<'a> {
        let mut header = self.header.clone();
        header.generation += 1;
        Partition {
            header,
            values: self
                .values
                .iter()
                .filter_map(|v| {
                    if v.header.state == VAR_ADDED {
                        Some(v.clone())
                    } else {
                        None
                    }
                })
                .collect(),
            empty_region_end: self.header.size(),
        }
    }
}

impl<'a> crate::Partition<'a> for Partition<'a> {
    fn get_variable(&self, key: &[u8], typ: VarType) -> Option<&dyn crate::Variable<'a>> {
        self.values.iter().find_map(|e| {
            if e.key == key && e.typ() == typ && e.header.state == VAR_ADDED {
                Some(e as &dyn crate::Variable<'a>)
            } else {
                None
            }
        })
    }

    fn insert_variable(&mut self, key: &[u8], value: Cow<'a, [u8]>, typ: VarType) {
        // invalidate any previous variable instances
        for var in self.entries_added(key, typ) {
            var.header.state = var.header.state & VAR_DELETED & VAR_IN_DELETED_TRANSITION;
        }

        let guid = match typ {
            VarType::Common => APPLE_COMMON_VARIABLE_GUID,
            VarType::System => APPLE_SYSTEM_VARIABLE_GUID,
        };
        let var = Variable {
            header: VarHeader {
                state: VAR_ADDED,
                attrs: 0,
                name_size: (key.len() + 1) as u32,
                data_size: value.len() as u32,
                guid,
                crc: crc32fast::hash(&value),
            },
            key: Cow::Owned(key.into()),
            value,
        };
        self.values.push(var);
    }

    fn remove_variable(&mut self, key: &[u8], typ: VarType) {
        // invalidate all previous variable instances
        for var in self.entries_added(key, typ) {
            var.header.state = var.header.state & VAR_DELETED & VAR_IN_DELETED_TRANSITION;
        }
    }

    fn variables(&self) -> Box<dyn Iterator<Item = &dyn crate::Variable<'a>> + '_> {
        Box::new(self.variables().map(|e| e as &dyn crate::Variable<'a>))
    }
}

impl Display for Partition<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "size: {}, total_used: {}, system_used: {}, common_used: {}, generation: 0x{:02x}, state: 0x{:02x}, flags: 0x{:02x}, count: {}",
            self.header.size,
            self.total_used(),
            self.system_used(),
            self.common_used(),
            self.generation(),
            self.header.state,
            self.header.flags,
            self.values.len()
        )
    }
}

#[derive(Debug, Clone)]
pub struct StoreHeader<'a> {
    pub name: &'a [u8],
    pub size: u32,
    pub generation: u32,
    pub state: u8,
    pub flags: u8,
    pub version: u8,
    pub system_size: u32,
    pub common_size: u32,
}

impl<'a> StoreHeader<'a> {
    fn parse(nvr: &[u8]) -> Result<StoreHeader<'_>> {
        let name = &nvr[..4];
        let size = u32::from_le_bytes(nvr[4..8].try_into().unwrap());
        let generation = u32::from_le_bytes(nvr[8..12].try_into().unwrap());
        let state = nvr[12];
        let flags = nvr[13];
        let version = nvr[14];
        let system_size = u32::from_le_bytes(nvr[16..20].try_into().unwrap());
        let common_size = u32::from_le_bytes(nvr[20..24].try_into().unwrap());

        if name != VARIABLE_STORE_SIGNATURE {
            return Err(V3Error::ParseError);
        }
        if version != VARIABLE_STORE_VERSION {
            return Err(V3Error::ParseError);
        }

        Ok(StoreHeader {
            name,
            size,
            generation,
            state,
            flags,
            version,
            system_size,
            common_size,
        })
    }

    fn serialize(&self, v: &mut Vec<u8>) {
        v.extend_from_slice(VARIABLE_STORE_SIGNATURE);
        v.extend_from_slice(&self.size.to_le_bytes());
        v.extend_from_slice(&self.generation.to_le_bytes());
        v.push(self.state);
        v.push(self.flags);
        v.push(self.version);
        v.push(0); // reserved
        v.extend_from_slice(&self.system_size.to_le_bytes());
        v.extend_from_slice(&self.common_size.to_le_bytes());
    }

    fn size(&self) -> usize {
        self.size as usize
    }
}

#[derive(Debug, Default, Clone)]
pub struct Variable<'a> {
    pub header: VarHeader<'a>,
    pub key: Cow<'a, [u8]>,
    pub value: Cow<'a, [u8]>,
}

impl<'a> Variable<'a> {
    fn size(&self) -> usize {
        VAR_HEADER_SIZE + (self.header.name_size + self.header.data_size) as usize
    }

    fn typ(&self) -> VarType {
        if self.header.guid == APPLE_SYSTEM_VARIABLE_GUID {
            return VarType::System;
        }
        VarType::Common
    }

    fn serialize(&self, v: &mut Vec<u8>) {
        self.header.serialize(v);
        v.extend_from_slice(&self.key);
        v.push(0);
        v.extend_from_slice(&self.value);
    }
}

impl<'a> crate::Variable<'a> for Variable<'a> {
    fn value(&self) -> Cow<'a, [u8]> {
        self.value.clone()
    }
}

impl Display for Variable<'_> {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        let key = String::from_utf8_lossy(&self.key);
        let mut value = String::new();
        for c in self.value.iter().copied() {
            if (c as char).is_ascii() && !(c as char).is_ascii_control() {
                value.push(c as char);
            } else {
                value.push_str(&format!("%{c:02x}"));
            }
        }

        write!(f, "{}:{}={}", self.typ(), key, value)
    }
}

#[derive(Debug, Default, Clone)]
pub struct VarHeader<'a> {
    pub state: u8,
    pub attrs: u32,
    pub name_size: u32,
    pub data_size: u32,
    pub guid: &'a [u8],
    pub crc: u32,
}

impl<'a> VarHeader<'a> {
    fn parse(nvr: &[u8]) -> Result<VarHeader<'_>> {
        let start_id = u16::from_le_bytes(nvr[..2].try_into().unwrap());
        if start_id != VARIABLE_DATA {
            return Err(V3Error::ParseError);
        }
        let state = nvr[2];
        let attrs = u32::from_le_bytes(nvr[4..8].try_into().unwrap());
        let name_size = u32::from_le_bytes(nvr[8..12].try_into().unwrap());
        let data_size = u32::from_le_bytes(nvr[12..16].try_into().unwrap());
        let guid = &nvr[16..32];
        let crc = u32::from_le_bytes(nvr[32..36].try_into().unwrap());

        if VAR_HEADER_SIZE + (name_size + data_size) as usize > nvr.len() {
            return Err(V3Error::ParseError);
        }

        Ok(VarHeader {
            state,
            attrs,
            name_size,
            data_size,
            guid,
            crc,
        })
    }

    fn serialize(&self, v: &mut Vec<u8>) {
        v.extend_from_slice(&VARIABLE_DATA.to_le_bytes());
        v.push(self.state);
        v.push(0); // reserved
        v.extend_from_slice(&self.attrs.to_le_bytes());
        v.extend_from_slice(&self.name_size.to_le_bytes());
        v.extend_from_slice(&self.data_size.to_le_bytes());
        v.extend_from_slice(self.guid);
        v.extend_from_slice(&self.crc.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Nvram as NvramT, NvramWriter, Partition};

    struct TestNvram {
        data: Vec<u8>,
        erase_count: usize,
    }

    impl TestNvram {
        fn new(data: Vec<u8>) -> TestNvram {
            Self {
                data,
                erase_count: 0,
            }
        }

        fn get_data(&self) -> &[u8] {
            &self.data
        }
    }

    impl NvramWriter for TestNvram {
        fn erase_if_needed(&mut self, offset: u32, size: usize) {
            for b in self.data.iter_mut().skip(offset as usize).take(size) {
                *b = 0xFF;
            }
            self.erase_count += 1;
        }

        fn write_all(&mut self, offset: u32, buf: &[u8]) -> std::io::Result<()> {
            for (d, s) in self
                .data
                .iter_mut()
                .skip(offset as usize)
                .take(buf.len())
                .zip(buf.iter().copied())
            {
                *d &= s;
            }
            Ok(())
        }
    }

    #[rustfmt::skip]
    fn store_header() -> &'static [u8] {
        &[
            0x33, 0x56, 0x56, 0x4e, 0x00, 0x00, 0x01, 0x00,
            0x01, 0x00, 0x00, 0x00, 0xfe, 0x5a, 0x01, 0x00,
            0x00, 0x40, 0x00, 0x00, 0x00, 0xc0, 0x00, 0x00,
        ]
    }

    fn empty_nvram(bank_count: usize) -> Vec<u8> {
        let mut data = vec![0xFF; PARTITION_SIZE * bank_count];
        data[0..STORE_HEADER_SIZE].copy_from_slice(store_header());
        data
    }

    #[test]
    fn test_insert_variable() -> crate::Result<()> {
        let mut nvr = TestNvram::new(empty_nvram(2));
        let data = nvr.get_data().to_owned();
        let mut nv = Nvram::parse(&data)?;

        assert!(matches!(nv.partitions[0], Slot::Valid(_)));
        assert!(matches!(nv.partitions[1], Slot::Empty));

        nv.active_part_mut().insert_variable(
            b"test-variable",
            Cow::Borrowed(b"test-value"),
            VarType::Common,
        );

        // write changes
        nv.apply(&mut nvr)?;
        assert_eq!(nvr.erase_count, 0);

        // try to parse again
        let data_after = nvr.get_data().to_owned();
        let mut nv_after = Nvram::parse(&data_after)?;

        let test_var = nv_after
            .active_part()
            .get_variable(b"test-variable", VarType::Common)
            .unwrap();

        assert_eq!(test_var.value(), Cow::Borrowed(b"test-value"));

        let test_var_entries: Vec<_> = nv_after
            .active_part_mut()
            .entries(b"test-variable", VarType::Common)
            .collect();

        assert_eq!(test_var_entries.len(), 1);
        assert_eq!(test_var_entries[0].header.state, VAR_ADDED);

        // update variable
        nv_after.active_part_mut().insert_variable(
            b"test-variable",
            Cow::Borrowed(b"test-value2"),
            VarType::Common,
        );

        // write changes
        nv_after.apply(&mut nvr)?;
        assert_eq!(nvr.erase_count, 0);

        // try to parse again
        let data_after2 = nvr.get_data().to_owned();
        let mut nv_after2 = Nvram::parse(&data_after2)?;

        let test_var2 = nv_after2
            .active_part()
            .get_variable(b"test-variable", VarType::Common)
            .unwrap();

        assert_eq!(test_var2.value(), Cow::Borrowed(b"test-value2"));

        let test_var2_entries: Vec<_> = nv_after2
            .active_part_mut()
            .entries(b"test-variable", VarType::Common)
            .collect();

        assert_eq!(test_var2_entries.len(), 2);
        assert_eq!(
            test_var2_entries[0].header.state,
            VAR_ADDED & VAR_DELETED & VAR_IN_DELETED_TRANSITION
        );
        assert_eq!(test_var2_entries[1].header.state, VAR_ADDED);

        Ok(())
    }

    #[test]
    fn test_write_to_next_bank() -> crate::Result<()> {
        let mut nvr = TestNvram::new(empty_nvram(2));
        // write something to the second bank to force nvram erase later
        nvr.data[0x10000..0x10007].copy_from_slice(b"garbage");

        let data = nvr.get_data().to_owned();
        let mut nv = Nvram::parse(&data)?;

        assert!(matches!(nv.partitions[0], Slot::Valid(_)));
        assert!(matches!(nv.partitions[1], Slot::Invalid));

        let orig_sys_val = vec![b'.'; 8192];
        nv.active_part_mut().insert_variable(
            b"test-large-variable",
            Cow::Borrowed(&orig_sys_val),
            VarType::System,
        );

        let orig_common_val = vec![b'.'; 24576];
        nv.active_part_mut().insert_variable(
            b"test-large-variable",
            Cow::Borrowed(&orig_common_val),
            VarType::Common,
        );

        // write changes
        nv.apply(&mut nvr)?;
        assert_eq!(nvr.erase_count, 0);
        assert_eq!(nv.active, 0);

        // try to parse again
        let data_after = nvr.get_data().to_owned();
        let mut nv_after = Nvram::parse(&data_after)?;
        assert_eq!(nv_after.active_part().header.generation, 1);

        assert!(matches!(nv_after.partitions[0], Slot::Valid(_)));
        assert!(matches!(nv_after.partitions[1], Slot::Invalid));

        // update variable
        let updated_sys_val = vec![b'.'; 9000];
        nv_after.active_part_mut().insert_variable(
            b"test-large-variable",
            Cow::Borrowed(&updated_sys_val),
            VarType::System,
        );

        let updated_common_val = vec![b'.'; 25000];
        nv_after.active_part_mut().insert_variable(
            b"test-large-variable",
            Cow::Borrowed(&updated_common_val),
            VarType::Common,
        );

        assert_eq!(nv_after.active_part().values.len(), 4);

        // write changes
        nv_after.apply(&mut nvr)?;
        assert_eq!(nvr.erase_count, 1);
        assert_eq!(nv_after.active, 1);
        assert_eq!(nv_after.active_part().values.len(), 2);

        // try to parse again
        let data_after2 = nvr.get_data().to_owned();
        let nv_after2 = Nvram::parse(&data_after2)?;
        assert_eq!(nv_after2.active, 1);
        assert_eq!(nv_after2.active_part().values.len(), 2);
        assert_eq!(nv_after2.active_part().header.generation, 2);

        assert!(matches!(nv_after2.partitions[0], Slot::Valid(_)));
        assert!(matches!(nv_after2.partitions[1], Slot::Valid(_)));

        let test_sys_var2 = nv_after2
            .active_part()
            .get_variable(b"test-large-variable", VarType::System)
            .unwrap();

        let test_common_var2 = nv_after2
            .active_part()
            .get_variable(b"test-large-variable", VarType::Common)
            .unwrap();

        assert_eq!(test_sys_var2.value(), Cow::Borrowed(&updated_sys_val));
        assert_eq!(test_common_var2.value(), Cow::Borrowed(&updated_common_val));

        let test_old_sys_var = nv_after2.partitions[0]
            .as_ref()
            .unwrap()
            .get_variable(b"test-large-variable", VarType::System)
            .unwrap();

        let test_old_common_var = nv_after2.partitions[0]
            .as_ref()
            .unwrap()
            .get_variable(b"test-large-variable", VarType::Common)
            .unwrap();

        assert_eq!(test_old_sys_var.value(), Cow::Borrowed(&orig_sys_val));
        assert_eq!(test_old_common_var.value(), Cow::Borrowed(&orig_common_val));

        Ok(())
    }

    #[test]
    fn test_insert_with_low_space() -> crate::Result<()> {
        let mut nvr = TestNvram::new(empty_nvram(2));
        // this will shrink usable size to 100 bytes
        nvr.data[STORE_HEADER_SIZE + 100] = 0x42;

        let data = nvr.get_data().to_owned();
        let mut nv = Nvram::parse(&data)?;

        assert!(matches!(nv.partitions[0], Slot::Valid(_)));
        assert!(matches!(nv.partitions[1], Slot::Empty));

        nv.active_part_mut().insert_variable(
            b"test-variable",
            Cow::Borrowed(b"test-value"),
            VarType::Common,
        );

        // write changes
        nv.apply(&mut nvr)?;
        assert_eq!(nvr.erase_count, 0);
        assert_eq!(nv.active, 0);

        // try to parse again
        let data_after = nvr.get_data().to_owned();
        let mut nv_after = Nvram::parse(&data_after)?;

        let test_var = nv_after
            .active_part()
            .get_variable(b"test-variable", VarType::Common)
            .unwrap();

        assert_eq!(test_var.value(), Cow::Borrowed(b"test-value"));

        let test_var_entries: Vec<_> = nv_after
            .active_part_mut()
            .entries(b"test-variable", VarType::Common)
            .collect();

        assert_eq!(test_var_entries.len(), 1);
        assert_eq!(test_var_entries[0].header.state, VAR_ADDED);

        // update variable
        nv_after.active_part_mut().insert_variable(
            b"test-variable",
            Cow::Borrowed(b"test-value2"),
            VarType::Common,
        );

        // write changes
        nv_after.apply(&mut nvr)?;
        assert_eq!(nvr.erase_count, 0);
        assert_eq!(nv_after.active, 1);

        Ok(())
    }
}

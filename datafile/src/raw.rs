use itertools::Itertools;
use log;
use num::ToPrimitive;
use std::mem;
use std::ops;
use std::str;
use zlib;

use bitmagic::CallbackExt;
use bitmagic::as_mut_i32_slice;
use bitmagic::relative_size_of;
use bitmagic::relative_size_of_mult;
use bitmagic::to_little_endian;
use bitmagic::transmute_slice;
use common::slice::mut_ref_slice;
use format::DATAFILE_ITEMTYPE_ID_RANGE;
use format::DatafileHeaderRest;
use format::DatafileHeaderVersion;
use format::DatafileItemHeader;
use format::DatafileItemType;
use format::OnlyI32;

pub trait Callback {
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize,CallbackError>;
    fn seek_read(&mut self, start: u32, buffer: &mut [u8]) -> Result<usize,CallbackError>;
    fn set_seek_base(&mut self) -> Result<(),CallbackError>;
    fn ensure_filesize(&mut self, filesize: u32) -> Result<Result<(),()>,CallbackError>;
    type Data: DataCallback;
    fn alloc_data(&mut self, length: usize) -> Result<Self::Data,CallbackError>;
}

pub trait DataCallback {
    fn slice_mut(&mut self) -> &mut [u8];
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DatafileHeader {
    pub hv: DatafileHeaderVersion,
    pub hr: DatafileHeaderRest,
}

pub struct ItemView<'a> {
    pub type_id: u16,
    pub id: u16,
    pub data: &'a [i32],
}

unsafe impl OnlyI32 for DatafileHeader { }

impl DatafileHeader {
    pub fn read<CB:Callback>(cb: &mut CB) -> Result<DatafileHeader,Error> {
        let mut result: DatafileHeader = unsafe { mem::uninitialized() };
        let read = try!(cb.read_le_i32s(mut_ref_slice(&mut result)));
        if read < mem::size_of_val(&result.hv) {
            return Err(Error::Df(DatafileError::TooShortHeaderVersion));
        }
        {
            let slice = as_mut_i32_slice(mut_ref_slice(&mut result));
            // Revert endian conversion for magic field.
            unsafe { to_little_endian(&mut slice[..1]); }
        }
        try!(result.hv.check());
        if read < mem::size_of_val(&result) {
            return Err(Error::Df(DatafileError::TooShortHeader));
        }
        try!(result.hr.check());
        try!(result.check());
        debug!("read header={:?}", result);
        Ok(result)
    }
    pub fn check(&self) -> Result<(),DatafileError> {
        let expected_size = try!(self.total_size());
        if self.hr.size != expected_size {
            error!("size does not match expected size, size={} expected={}", self.hr.size, expected_size);
        } else {
            return Ok(())
        }
        Err(DatafileError::MalformedHeader)
    }
    pub fn total_size(&self) -> Result<i32,DatafileError> {
        // These two functions are just used to make the lines in this function
        // shorter. `u` converts an `i32` to an `u64`, and `s` returns the size
        // of the type as `u64`.
        fn u(val: i32) -> u64 { val.to_u64().unwrap() }
        fn s<T>() -> u64 { mem::size_of::<T>().to_u64().unwrap() }

        let result: u64
            // The whole computation won't overflow because we're multiplying
            // small integers with `u32`s.
            = s::<DatafileHeaderRest>() - s::<i32>() * 2 // header_rest without size, _swaplen
            + s::<DatafileItemType>() * u(self.hr.num_item_types) // item_types
            + s::<i32>() * u(self.hr.num_items) // item_offsets
            + s::<i32>() * u(self.hr.num_data) // data_offsets
            + if self.hv.version >= 4 { s::<i32>() * u(self.hr.num_data) } else { 0 } // data_sizes (only version 4)
            + u(self.hr.size_items) // items
            + u(self.hr.size_data); // data

        result.to_i32().ok_or(DatafileError::MalformedHeader)
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq, Debug)]
pub enum CallbackReadError {
    Cb(CallbackError),
    EndOfFile,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq, Debug)]
pub struct CallbackError;

#[derive(Clone, Copy, Eq, Hash, PartialEq, Debug)]
pub enum DatafileError {
    WrongMagic([u8; 4]),
    UnsupportedVersion(i32),
    MalformedHeader,
    Malformed,
    CompressionError,
    TooShort,
    TooShortHeaderVersion,
    TooShortHeader,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq, Debug)]
pub enum Error {
    Df(DatafileError),
    Cb(CallbackError),
}

impl From<DatafileError> for Error {
    fn from(err: DatafileError) -> Error {
        Error::Df(err)
    }
}

impl From<CallbackError> for Error {
    fn from(err: CallbackError) -> Error {
        Error::Cb(err)
    }
}

impl From<CallbackError> for CallbackReadError {
    fn from(err: CallbackError) -> CallbackReadError {
        CallbackReadError::Cb(err)
    }
}

impl CallbackReadError {
    pub fn on_eof(self, df_err: DatafileError) -> Error {
        match self {
            CallbackReadError::Cb(err) => From::from(err),
            CallbackReadError::EndOfFile => From::from(df_err),
        }
    }
}

pub struct Reader {
    header: DatafileHeader,
    item_types: Vec<DatafileItemType>,
    item_offsets: Vec<i32>,
    data_offsets: Vec<i32>,
    uncomp_data_sizes: Option<Vec<i32>>,
    items_raw: Vec<i32>,
}

impl Reader {
    pub fn new<CB:Callback>(cb: &mut CB) -> Result<Reader,Error> {
        fn read_i32s<CB:Callback,T:OnlyI32>(cb: &mut CB, len: usize) -> Result<Vec<T>,Error> {
            cb.read_exact_le_i32s_owned::<T>(len).map_err(|e| e.on_eof(DatafileError::TooShort))
        }

        let header = try!(DatafileHeader::read(cb));
        let item_types_raw = try!(read_i32s(cb, header.hr.num_item_types as usize));
        let item_offsets = try!(read_i32s(cb, header.hr.num_items as usize));
        let data_offsets = try!(read_i32s(cb, header.hr.num_data as usize));
        let uncomp_data_sizes = match header.hv.version {
            3 => None,
            4 => Some(try!(read_i32s(cb, header.hr.num_data as usize))),
            _ => unreachable!(), // Should have been caught in header.check().
        };
        // Possible failure of relative_size_of_mult should have been caught in header.check().
        let items_raw = try!(read_i32s(cb, relative_size_of_mult::<u8,i32>(header.hr.size_items as usize)));

        try!(cb.set_seek_base());

        let result = Reader {
            header: header,
            item_types: item_types_raw,
            item_offsets: item_offsets,
            data_offsets: data_offsets,
            uncomp_data_sizes: uncomp_data_sizes,
            items_raw: items_raw,
        };
        try!(result.check());
        Ok(result)
    }
    pub fn check(&self) -> Result<(),DatafileError> {
        {
            let mut expected_start = 0;
            for (i, t) in self.item_types.iter().enumerate() {
                if !(0 <= t.type_id && t.type_id < DATAFILE_ITEMTYPE_ID_RANGE) {
                    error!("invalid item_type type_id: must be in range 0 to {:x}, item_type={} type_id={}", DATAFILE_ITEMTYPE_ID_RANGE, i, t.type_id);
                    return Err(DatafileError::Malformed);
                }
                if !(0 <= t.num && t.num <= self.header.hr.num_items - t.start) {
                    error!("invalid item_type num: must be in range 0 to num_items - start + 1, item_type={} type_id={} start={} num={}", i, t.type_id, t.start, t.num);
                    return Err(DatafileError::Malformed);
                }
                if t.start != expected_start {
                    error!("item_types are not sequential, item_type={} type_id={} start={} expected={}", i, t.type_id, t.start, expected_start);
                    return Err(DatafileError::Malformed);
                }
                expected_start += t.num;
                for (k, t2) in self.item_types[..i].iter().enumerate() {
                    if t.type_id == t2.type_id {
                        error!("item_type type_id occurs twice, type_id={} item_type1={} item_type2={}", t.type_id, i, k);
                        return Err(DatafileError::Malformed);
                    }
                }
            }
            if expected_start != self.header.hr.num_items {
                error!("last item_type does not contain last item, item_type={}", self.header.hr.num_item_types - 1);
                return Err(DatafileError::Malformed);
            }
        }
        {
            let mut offset = 0;
            for i in 0..self.header.hr.num_items as usize {
                if self.item_offsets[i] < 0 {
                    error!("invalid item offset (negative), item={} offset={}", i, self.item_offsets[i]);
                    return Err(DatafileError::Malformed);
                }
                if offset != self.item_offsets[i] as usize {
                    error!("invalid item offset, item={} offset={} wanted={}", i, self.item_offsets[i], offset);
                    return Err(DatafileError::Malformed);
                }
                offset += mem::size_of::<DatafileItemHeader>();
                if offset > self.header.hr.size_items as usize {
                    error!("item header out of bounds, item={} offset={} size_items={}", i, offset, self.header.hr.size_items);
                    return Err(DatafileError::Malformed);
                }
                let item_header = self.item_header(i);
                if item_header.size < 0 {
                    error!("item has negative size, item={} size={}", i, item_header.size);
                    return Err(DatafileError::Malformed);
                }
                offset += item_header.size as usize;
                if offset > self.header.hr.size_items as usize {
                    error!("item out of bounds, item={} size={} size_items={}", i, item_header.size, self.header.hr.size_items);
                    return Err(DatafileError::Malformed);
                }
            }
            if offset != self.header.hr.size_items as usize {
                error!("last item not large enough, item={} offset={} size_items={}", self.header.hr.num_items - 1, offset, self.header.hr.size_items);
                return Err(DatafileError::Malformed);
            }
        }
        {
            let mut previous = 0;
            for i in 0..self.header.hr.num_data as usize {
                match self.uncomp_data_sizes {
                    Some(ref uds) => {
                        if uds[i] < 0 {
                            error!("invalid data's uncompressed size, data={} uncomp_data_size={}", i, uds[i]);
                            return Err(DatafileError::Malformed);
                        }
                    }
                    None => (),
                }
                let offset = self.data_offsets[i];
                if offset < 0 || offset > self.header.hr.size_data {
                    error!("invalid data offset, data={} offset={}", i, offset);
                    return Err(DatafileError::Malformed);
                }
                if previous > offset {
                    error!("data overlaps, data1={} data2={}", i - 1, i);
                    return Err(DatafileError::Malformed);
                }
                previous = offset;
            }
        }
        {
            for (i, t) in self.item_types.iter().enumerate() {
                for k in t.start as usize..(t.start + t.num) as usize {
                    let item_header = self.item_header(k);
                    if item_header.type_id() != t.type_id as u16 {
                        error!("item does not have right type_id, type={} type_id1={} item={} type_id2={}", i, t.type_id, k, item_header.type_id());
                        return Err(DatafileError::Malformed);
                    }
                }
            }
        }
        Ok(())
    }
    fn item_header(&self, index: usize) -> &DatafileItemHeader {
        let slice = &self.items_raw
            [relative_size_of_mult::<u8,i32>(self.item_offsets[index].to_usize().unwrap())..]
            [..relative_size_of::<DatafileItemHeader,i32>()];
        // TODO: Find out why paranthesis are necessary.
        //
        // This operation is safe because both `i32` and `DatafileItemHeader`
        // are POD.
        &(unsafe { transmute_slice::<i32,DatafileItemHeader>(slice) })[0]
    }
    fn data_size_file(&self, index: usize) -> usize {
        let start = self.data_offsets[index] as usize;
        let end = if index < self.data_offsets.len() - 1 {
            self.data_offsets[index + 1] as usize
        } else {
            self.header.hr.size_data as usize
        };
        assert!(start <= end);
        end - start
    }
    pub fn read_data<CB:Callback>(&self, cb: &mut CB, index: usize) -> Result<CB::Data,Error> {
        let raw_data_len = self.data_size_file(index);
        let raw_data = try!(cb.seek_read_exact_owned(self.data_offsets[index] as u32, raw_data_len).map_err(|e| e.on_eof(DatafileError::TooShort)));

        match self.uncomp_data_sizes {
            Some(ref uds) => {
                let data_len = uds[index] as usize;
                let mut data = try!(cb.alloc_data(data_len));

                match zlib::uncompress(data.slice_mut(), &raw_data) {
                    Ok(len) if len == data_len => {
                        Ok(data)
                    }
                    Ok(len) => {
                        error!("decompression error: wrong size, data={} size={} wanted={}", index, data_len, len);
                        Err(From::from(DatafileError::CompressionError))
                    }
                    _ => {
                        error!("decompression error: zlib error");
                        Err(From::from(DatafileError::CompressionError))
                    }
                }
            },
            None => {
                let data_len = raw_data_len;
                let mut data = try!(cb.alloc_data(data_len));
                data.slice_mut().iter_mut().set_from(raw_data.iter().cloned());
                Ok(data)
            },
        }
    }
    pub fn item(&self, index: usize) -> ItemView {
        let item_header = self.item_header(index);
        let data = &self.items_raw
            [relative_size_of_mult::<u8,i32>(self.item_offsets[index].to_usize().unwrap())..]
            [relative_size_of::<DatafileItemHeader,i32>()..]
            [..relative_size_of_mult::<u8,i32>(item_header.size.to_usize().unwrap())];
        ItemView {
            type_id: item_header.type_id(),
            id: item_header.id(),
            data: data,
        }
    }
    pub fn num_items(&self) -> usize {
        self.header.hr.num_items.to_usize().unwrap()
    }
    pub fn num_data(&self) -> usize {
        self.header.hr.num_data.to_usize().unwrap()
    }
    pub fn item_type_indices(&self, type_id: u16) -> ops::Range<usize> {
        for t in self.item_types.iter() {
            if t.type_id as u16 == type_id {
                let start = t.start.to_usize().unwrap();
                let num = t.num.to_usize().unwrap();
                return start..start+num;
            }
        }
        0..0
    }
    pub fn item_type(&self, index: usize) -> u16 {
        self.item_types[index].type_id.to_u16().unwrap()
    }
    pub fn num_item_types(&self) -> usize {
        self.header.hr.num_item_types.to_usize().unwrap()
    }

    pub fn debug_dump<CB:Callback>(&self, cb: &mut CB) -> Result<(),Error> {
        if !log_enabled!(log::LogLevel::Debug) {
            return Ok(())
        }
        debug!("DATAFILE");
        debug!("header: {:?}", self.header);

        for type_id in self.item_types() {
            debug!("item_type type_id={}", type_id);
            for item in self.item_type_items(type_id) {
                debug!("\titem id={} data={:?}", item.id, item.data);
            }
        }
        for (i, data) in self.data_iter(cb).enumerate() {
            let mut data = try!(data);
            let len = data.slice_mut().len();
            debug!("data id={} size={}", i, len);
            if len < 256 {
                match str::from_utf8(data.slice_mut()).ok() {
                    Some(s) => debug!("\tstr={:?}", s),
                    None => {},
                }
            }
        }
        Ok(())
    }

    pub fn items(&self) -> Items {
        fn map_fn<'a>(i: usize, self_: &mut &'a Reader) -> ItemView<'a> {
            self_.item(i)
        }
        MapIterator {
            data: self,
            iterator: 0..self.num_items(),
            map_fn: map_fn,
        }
    }
    pub fn item_types(&self) -> ItemTypes {
        fn map_fn<'a>(i: usize, self_: &mut &'a Reader) -> u16 {
            self_.item_type(i)
        }
        MapIterator {
            data: self,
            iterator: 0..self.num_item_types(),
            map_fn: map_fn,
        }
    }
    pub fn item_type_items(&self, type_id: u16) -> ItemTypeItems {
        fn map_fn<'a>(i: usize, self_: &mut &'a Reader) -> ItemView<'a> {
            self_.item(i)
        }
        MapIterator {
            data: self,
            iterator: self.item_type_indices(type_id),
            map_fn: map_fn,
        }
    }
    pub fn data_iter<'a,CB:Callback>(&'a self, cb: &'a mut CB) -> DataIter<'a,CB,CB::Data> {
        fn map_fn<CB:Callback>(i: usize, &mut (self_, ref mut cb): &mut (&Reader, &mut CB)) -> Result<CB::Data,Error> {
            self_.read_data(*cb, i)
        }
        MapIterator {
            data: (self, cb),
            iterator: 0..self.num_data(),
            map_fn: map_fn,
        }
    }
}

pub type DataIter<'a,CB,T> = MapIterator<Result<T,Error>,(&'a Reader,&'a mut CB),ops::Range<usize>>;
pub type Items<'a> = MapIterator<ItemView<'a>,&'a Reader,ops::Range<usize>>;
pub type ItemTypes<'a> = MapIterator<u16,&'a Reader,ops::Range<usize>>;
pub type ItemTypeItems<'a> = MapIterator<ItemView<'a>,&'a Reader,ops::Range<usize>>;

pub struct MapIterator<T,D,I:Iterator> {
    data: D,
    iterator: I,
    // `map` is already an function of an iterator, so we can't use `map` as a
    // name here.
    map_fn: fn (I::Item, &mut D) -> T,
}

impl<T,D,I:Iterator> Iterator for MapIterator<T,D,I> {
    type Item = T;
    fn next(&mut self) -> Option<T> {
        self.iterator.next().map(|x| (self.map_fn)(x, &mut self.data))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.iterator.size_hint()
    }
}


#[derive(Clone, Copy, Debug)]
struct DfBufItemType {
    type_id: u16,
    start: usize,
    num: usize,
}

#[derive(Clone, Debug)]
struct DfBufItem {
    type_id: u16,
    id: u16,
    data: Vec<i32>,
}

pub struct DatafileBuffer {
    item_types: Vec<DfBufItemType>,
    items: Vec<DfBufItem>,
    data: Vec<Vec<u8>>,
}

pub type DfDataNoerrIter<'a,T> = MapIterator<&'a [u8],&'a T,ops::Range<usize>>;

//fn datafile_data_noerr_map_fn<'a>(index: usize, df: &&'a DatafileBuffer) -> &'a [u8] {
//    df.data_noerr(index)
//}

impl DatafileBuffer {
    pub fn new() -> DatafileBuffer {
        DatafileBuffer {
            item_types: Vec::new(),
            items: Vec::new(),
            data: Vec::new(),
        }
    }

    fn get_item_type_index(&self, type_id: u16) -> (usize, bool) {
        for (i, &DfBufItemType { type_id: other_type_id, .. }) in self.item_types.iter().enumerate() {
            if type_id <= other_type_id {
                return (i, type_id == other_type_id);
            }
        }
        (self.item_types.len(), false)
    }

    fn get_item_index(&self, item_type_index: usize, item_type_found: bool, id: u16) -> (usize, bool) {
        if !item_type_found {
            if item_type_index != self.item_types.len() {
                (self.item_types[item_type_index].start, false)
            } else {
                (self.items.len(), false)
            }
        } else {
            let DfBufItemType { start, num, .. } = self.item_types[item_type_index];

            for (i, &DfBufItem { id: other_id, .. })
                in self.items[start..][..num].iter().enumerate().map(|(i, x)| (start+i, x)) {

                if id <= other_id {
                    return (i, id == other_id)
                }
            }

            (start + num, false)
        }
    }

    pub fn data_noerr<'a>(&'a self, index: usize) -> &'a [u8] {
        &self.data[index]
    }

    pub fn data_noerr_iter<'a>(&'a self) -> DfDataNoerrIter<'a,DatafileBuffer> {
        // TODO: Implement this!
        unimplemented!();
        //MapIterator { data: self, iterator: 0..self.num_data(), map_fn: datafile_data_noerr_map_fn }
    }

    pub fn add_item(&mut self, type_id: u16, id: u16, data: &[i32]) -> Result<(),()> {
        let (type_index, type_found) = self.get_item_type_index(type_id);
        let (item_index, item_found) = self.get_item_index(type_index, type_found, id);

        // if we already have an item of the given type and id,
        // return an error
        if item_found {
            return Err(());
        }

        // if there isn't a type with such an id yet, insert it
        if !type_found {
            self.item_types.insert(type_index, DfBufItemType {
                type_id: type_id,
                start: item_index,
                num: 0,
            });
        }

        // we're going to insert an item, increase the count by one
        self.item_types[type_index].num += 1;

        // increase the starts of the following item types by one
        for t in self.item_types.iter_mut().skip(type_index + 1) {
            t.start += 1;
        }

        // actually insert the item
        self.items.insert(item_index, DfBufItem {
            type_id: type_id,
            id: id,
            data: data.to_vec(),
        });

        Ok(())
    }

    pub fn add_data(&mut self, data: Vec<u8>) -> usize {
        // add the data
        self.data.push(data);
        // return the index
        self.data.len() - 1
    }
}
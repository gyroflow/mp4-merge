use std::{collections::BTreeMap, io::*};
use byteorder::{ LittleEndian, ReadBytesExt, WriteBytesExt };
use crate::writer::get_first;

pub const HEADER_SIZE: usize = 32 + 4 + 4 + 32; // padding(32), size(4), version(4), magic(32)
pub const MAGIC: &[u8] = b"8db42d694ccc418790edff439fe026bf";

pub fn get_insta360_offsets<R: Read + Seek>(files: &mut [(R, usize)]) -> Result<Vec<BTreeMap<u64, (u32, u8, u8, i64)>>> {
    let mut ret = Vec::new();
    for (ref mut stream, size) in files {

        let mut buf = vec![0u8; HEADER_SIZE];
        stream.seek(SeekFrom::End(-(HEADER_SIZE as i64)))?;
        stream.read_exact(&mut buf)?;
        let mut offsets = BTreeMap::new();
        if &buf[HEADER_SIZE-32..] == MAGIC {
            let extra_size = (&buf[32..]).read_u32::<LittleEndian>()? as i64;
            let data_version = (&buf[36..]).read_u32::<LittleEndian>()?;
            let extra_start  = *size - extra_size as usize;

            let mut offset = (HEADER_SIZE + 4+1+1) as i64;

            stream.seek(SeekFrom::End(-offset + 1))?;
            let first_id = stream.read_u8()?;
            if first_id == 0 { // record::RecordType::Offsets
                let size = stream.read_u32::<LittleEndian>()? as i64;
                buf.resize(size as usize, 0);
                stream.seek(SeekFrom::End(-offset - size))?;
                stream.read_exact(&mut buf)?;

                { // Parse offsets record
                    let len = buf.len() as u64;
                    let mut d = Cursor::new(buf.clone());

                    while d.position() < len as u64 {
                        let id     = d.read_u8()?;
                        let format = d.read_u8()?;
                        let size   = d.read_u32::<LittleEndian>()? as i64;
                        let offset = d.read_u32::<LittleEndian>()?;
                        if id > 0 {
                            offsets.insert(extra_start as u64 + offset as u64, (data_version, id, format, size));
                        }
                    }
                }
            } else {
                while offset < extra_size {
                    stream.seek(SeekFrom::End(-offset))?;

                    let format = stream.read_u8()?;
                    let id     = stream.read_u8()?;
                    let size   = stream.read_u32::<LittleEndian>()? as i64;

                    buf.resize(size as usize, 0);

                    stream.seek(SeekFrom::End(-offset - size))?;
                    if id > 0 {
                        offsets.insert(stream.stream_position()?, (data_version, id, format, size));
                    }

                    offset += size + 4+1+1;
                }
            }
        }
        ret.push(offsets);
    }
    Ok(ret)
}

pub fn merge_metadata<R: Read + Seek, W: Write + Seek>(files: &mut [(R, usize)], offsets: &[BTreeMap<u64, (u32, u8, u8, i64)>], mut f_out: W) -> Result<()> {
    assert_eq!(files.len(), offsets.len());

    let mut total_size = 0;
    let mut data_version = 3;

    for (offset, (ver, id, format, size)) in offsets.first().unwrap() {
        data_version = *ver;
        let first_stream = get_first(files);
        first_stream.seek(SeekFrom::Start(*offset))?;
        std::io::copy(&mut first_stream.take(*size as u64), &mut f_out)?;

        let format2 = first_stream.read_u8()?;
        let id2     = first_stream.read_u8()?;
        let mut size2 = first_stream.read_u32::<LittleEndian>()? as i64;

        if *id != id2 || *format != format2 || *size != size2 {
            return Err(Error::new(ErrorKind::InvalidData, "Invalid metadata"));
        }

        if id2 != 0 && id2 != 1 && id2 != 2 && id2 != 5 { // If not Offsets, Metadata, Thumbnail, ThumbnailExt
            // Merge binary data
            for (file_i, map) in offsets.iter().enumerate() {
                if file_i == 0 { continue; }
                for (offset, (_ver, id, _format, size)) in map {
                    if id2 == *id {
                        let stream_i = files.get_mut(file_i).map(|x| &mut x.0).unwrap();
                        stream_i.seek(SeekFrom::Start(*offset))?;
                        std::io::copy(&mut stream_i.take(*size as u64), &mut f_out)?;
                        size2 += *size as i64;
                    }
                }
            }
        }
        f_out.write_u8(format2)?;
        f_out.write_u8(id2)?;
        f_out.write_u32::<LittleEndian>(size2 as u32)?;
        total_size += size2 + 1+1+4;
    }

    f_out.write_u128::<LittleEndian>(0)?; // padding
    f_out.write_u128::<LittleEndian>(0)?; // padding
    f_out.write_u32::<LittleEndian>(total_size as u32 + 72)?;
    f_out.write_u32::<LittleEndian>(data_version)?; // version
    f_out.write(MAGIC)?;

    Ok(())
}

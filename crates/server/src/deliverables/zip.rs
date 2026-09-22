//! Minimal STORE-mode ZIP writer — port of `buildZip` in deliverables.ts.

pub struct ZipEntry {
    pub name: String,
    pub data: Vec<u8>,
}

const DOS_TIME: u16 = 0;
const DOS_DATE: u16 = 0x21;

static CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut n = 0u32;
    while n < 256 {
        let mut c = n;
        let mut k = 0;
        while k < 8 {
            c = if (c & 1) != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[n as usize] = c;
        n += 1;
    }
    table
};

pub fn crc32(buf: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in buf {
        let idx = ((crc ^ u32::from(b)) & 0xff) as usize;
        crc = CRC_TABLE[idx] ^ (crc >> 8);
    }
    crc ^ 0xffff_ffffu32
}

pub fn build_zip(entries: &[ZipEntry]) -> Vec<u8> {
    let mut parts: Vec<Vec<u8>> = Vec::new();
    let mut central: Vec<Vec<u8>> = Vec::new();
    let mut offset: u32 = 0;

    for entry in entries {
        let name_buf = entry.name.as_bytes();
        let crc = crc32(&entry.data);
        let size = entry.data.len() as u32;

        let mut local = vec![0u8; 30];
        local[0..4].copy_from_slice(&0x0403_4b50u32.to_le_bytes());
        local[4..6].copy_from_slice(&20u16.to_le_bytes());
        local[10..12].copy_from_slice(&DOS_TIME.to_le_bytes());
        local[12..14].copy_from_slice(&DOS_DATE.to_le_bytes());
        local[14..18].copy_from_slice(&crc.to_le_bytes());
        local[18..22].copy_from_slice(&size.to_le_bytes());
        local[22..26].copy_from_slice(&size.to_le_bytes());
        local[26..28].copy_from_slice(&(name_buf.len() as u16).to_le_bytes());

        parts.push(local);
        parts.push(name_buf.to_vec());
        parts.push(entry.data.clone());

        let mut central_header = vec![0u8; 46];
        central_header[0..4].copy_from_slice(&0x0201_4b50u32.to_le_bytes());
        central_header[4..6].copy_from_slice(&20u16.to_le_bytes());
        central_header[6..8].copy_from_slice(&20u16.to_le_bytes());
        central_header[12..14].copy_from_slice(&DOS_TIME.to_le_bytes());
        central_header[14..16].copy_from_slice(&DOS_DATE.to_le_bytes());
        central_header[16..20].copy_from_slice(&crc.to_le_bytes());
        central_header[20..24].copy_from_slice(&size.to_le_bytes());
        central_header[24..28].copy_from_slice(&size.to_le_bytes());
        central_header[28..30].copy_from_slice(&(name_buf.len() as u16).to_le_bytes());
        central_header[42..46].copy_from_slice(&offset.to_le_bytes());
        central.push(central_header);
        central.push(name_buf.to_vec());

        offset += 30 + name_buf.len() as u32 + size;
    }

    let central_offset = offset;
    let central_buf: Vec<u8> = central.into_iter().flatten().collect();

    let mut eocd = vec![0u8; 22];
    eocd[0..4].copy_from_slice(&0x0605_4b50u32.to_le_bytes());
    eocd[8..10].copy_from_slice(&(entries.len() as u16).to_le_bytes());
    eocd[10..12].copy_from_slice(&(entries.len() as u16).to_le_bytes());
    eocd[12..16].copy_from_slice(&(central_buf.len() as u32).to_le_bytes());
    eocd[16..20].copy_from_slice(&central_offset.to_le_bytes());

    parts
        .into_iter()
        .flatten()
        .chain(central_buf)
        .chain(eocd)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn zip_has_local_and_central_directory() {
        let zip = build_zip(&[
            ZipEntry {
                name: "a.txt".into(),
                data: b"hello".to_vec(),
            },
            ZipEntry {
                name: "dir/b.txt".into(),
                data: b"world".to_vec(),
            },
        ]);
        assert_eq!(&zip[0..4], &[0x50, 0x4b, 0x03, 0x04]);
        assert!(zip.windows(4).any(|w| w == [0x50, 0x4b, 0x01, 0x02]));
        assert!(zip.windows(4).any(|w| w == [0x50, 0x4b, 0x05, 0x06]));
    }
}

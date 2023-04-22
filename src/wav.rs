use std::mem;

pub const HEADER_SIZE: usize = 44;

// http://soundfile.sapp.org/doc/WaveFormat
pub fn package(pcm: &[i16]) -> Vec<u8> {
    let mut data = Vec::with_capacity(HEADER_SIZE + pcm.len() * 2);
    data.extend_from_slice("RIFF".as_bytes());
    data.extend_from_slice(&((pcm.len() * 2 + HEADER_SIZE - 8) as u32).to_le_bytes()); // Total length without data to this point
    data.extend_from_slice("WAVE".as_bytes());
    data.extend_from_slice("fmt ".as_bytes());
    data.extend_from_slice(&(16u32.to_le_bytes()));
    data.extend_from_slice(&(1u16.to_le_bytes())); // PCM
    data.extend_from_slice(&(1u16.to_le_bytes())); // 1 channel (mono)
    data.extend_from_slice(&(crate::storage::FREQUENCY as u32).to_le_bytes());
    data.extend_from_slice(
        &((crate::storage::FREQUENCY * mem::size_of::<i16>()) as u32).to_le_bytes(),
    ); // FREQUENCY * 1 (mono) * 16 / 8
    data.extend_from_slice(&(mem::size_of::<i16>() as u16).to_le_bytes()); // 1 (mono) * 16 / 8
    data.extend_from_slice(&((u16::BITS as u16).to_le_bytes())); // Bits per sample
    data.extend_from_slice("data".as_bytes());
    data.extend_from_slice(&(((pcm.len() * 2) as u32).to_le_bytes())); // PCM data length
    data.extend(pcm.into_iter().flat_map(|n| [*n as u8, (n >> 8) as u8]));

    data
}

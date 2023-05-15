use std::time::Duration;

pub const HEADER_SIZE: usize = 44;
pub const BITS_PER_SAMPLE: u32 = i16::BITS;
pub const CHANNELS: u16 = 1;

// http://soundfile.sapp.org/doc/WaveFormat
pub fn package(pcm: &[i16]) -> Vec<u8> {
    let mut data = Vec::with_capacity(HEADER_SIZE + pcm.len() * 2);
    data.extend_from_slice("RIFF".as_bytes());
    data.extend_from_slice(&((pcm.len() * 2 + HEADER_SIZE - 8) as u32).to_le_bytes()); // Total length without data up to this point
    data.extend_from_slice("WAVE".as_bytes());
    data.extend_from_slice("fmt ".as_bytes());
    data.extend_from_slice(&(16u32.to_le_bytes())); // Size of sub-chunk
    data.extend_from_slice(&(1u16.to_le_bytes())); // PCM format
    data.extend_from_slice(&(CHANNELS.to_le_bytes()));
    data.extend_from_slice(&(crate::recorder::FREQUENCY as u32).to_le_bytes());
    data.extend_from_slice(
        &(crate::recorder::FREQUENCY as u32 * CHANNELS as u32 * BITS_PER_SAMPLE / 8).to_le_bytes(),
    );
    data.extend_from_slice(&(CHANNELS * BITS_PER_SAMPLE as u16 / 8).to_le_bytes());
    data.extend_from_slice(&(BITS_PER_SAMPLE as u16).to_le_bytes());
    data.extend_from_slice("data".as_bytes());
    data.extend_from_slice(&(((pcm.len() * 2) as u32).to_le_bytes())); // PCM data length
    data.extend(pcm.into_iter().flat_map(|n| [*n as u8, (n >> 8) as u8]));

    data
}

pub fn is_valid_wav(data: &[u8]) -> bool {
    if data.len() < HEADER_SIZE {
        return false;
    }
    // TODO: Check frequency and co.
    &data[0..4] == b"RIFF" && &data[12..16] == b"WAVE" && &data[16..20] == b"fmt "
}

pub fn wav_duration_from_size(size: usize) -> Duration {
    if size < HEADER_SIZE {
        return Duration::from_secs(0);
    }
    // Multiply by 1_000 and use milliseconds to gain in precision.
    Duration::from_millis(((size - HEADER_SIZE) / 2 * 1_000 / crate::recorder::FREQUENCY) as u64)
}

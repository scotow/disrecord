use std::time::Duration;

pub const HEADER_SIZE: usize = 44;

const HEADERS_TEMPLATE: [&[u8]; 2] = [
    &[82, 73, 70, 70],
    &[
        87, 65, 86, 69, 102, 109, 116, 32, 16, 0, 0, 0, 1, 0, 1, 0, 128, 187, 0, 0, 0, 119, 1, 0,
        2, 0, 16, 0, 100, 97, 116, 97,
    ],
];

pub fn package(pcm: &[i16]) -> Vec<u8> {
    let mut data = Vec::with_capacity(HEADER_SIZE + pcm.len() * 2);
    data.extend_from_slice(HEADERS_TEMPLATE[0]);
    data.extend_from_slice(&((pcm.len() * 2 + HEADER_SIZE - 8) as u32).to_le_bytes()); // Total length without data up to this point
    data.extend_from_slice(HEADERS_TEMPLATE[1]);
    data.extend_from_slice(&(((pcm.len() * 2) as u32).to_le_bytes())); // PCM data length
    data.extend(pcm.iter().flat_map(|n| [*n as u8, (n >> 8) as u8]));
    data
}

/// Remove the WAV header while keeping its payload unchanged (little endian).
/// Panics if the slice is not long enough to have data.
pub fn remove_header(wav: &[u8]) -> &[u8] {
    &wav[HEADER_SIZE..]
}

pub fn is_valid(data: &[u8]) -> bool {
    if data.len() < HEADER_SIZE {
        return false;
    }
    &data[0..4] == HEADERS_TEMPLATE[0]
        && data[4..8] == ((data.len() - 8) as u32).to_le_bytes()
        && &data[8..40] == HEADERS_TEMPLATE[1]
        && data[40..44] == ((data.len() - HEADER_SIZE) as u32).to_le_bytes()
}

pub fn duration_from_size(size: usize) -> Duration {
    if size < HEADER_SIZE {
        return Duration::from_secs(0);
    }
    // Multiply by 1_000 and use milliseconds to gain in precision.
    Duration::from_millis(((size - HEADER_SIZE) / 2 * 1_000 / crate::recorder::FREQUENCY) as u64)
}

#[cfg(test)]
mod tests {
    use std::{iter, time::Duration};

    use itertools::Itertools;
    use rand::random;

    const BITS_PER_SAMPLE: u32 = i16::BITS;
    const CHANNELS: u16 = 1;

    use super::HEADER_SIZE;

    #[test]
    fn package() {
        let pcm = iter::repeat_with(|| random())
            .take(64 + random::<usize>() % 64)
            .collect_vec();

        // http://soundfile.sapp.org/doc/WaveFormat
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
            &(crate::recorder::FREQUENCY as u32 * CHANNELS as u32 * BITS_PER_SAMPLE / 8)
                .to_le_bytes(),
        );
        data.extend_from_slice(&(CHANNELS * BITS_PER_SAMPLE as u16 / 8).to_le_bytes());
        data.extend_from_slice(&(BITS_PER_SAMPLE as u16).to_le_bytes());
        data.extend_from_slice("data".as_bytes());
        data.extend_from_slice(&(((pcm.len() * 2) as u32).to_le_bytes())); // PCM data length
        data.extend(pcm.iter().flat_map(|n| n.to_le_bytes()));

        assert_eq!(super::package(&pcm), data);
        assert!(super::is_valid(&data));
        assert!(super::is_valid(&super::package(&pcm)));
    }

    #[test]
    fn validate() {
        assert!(super::is_valid(include_bytes!("hello.wav")));
    }

    #[test]
    fn duration() {
        assert_eq!(
            super::duration_from_size(include_bytes!("hello.wav").len()),
            Duration::from_millis(1120)
        );
    }
}

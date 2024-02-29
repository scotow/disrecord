use std::time::Duration;

pub const HEADER_SIZE: usize = 44;

const HEADER_TEMPLATES: [&[u8]; 2] = [
    &[82, 73, 70, 70],
    &[
        87, 65, 86, 69, 102, 109, 116, 32, 16, 0, 0, 0, 1, 0, 1, 0, 128, 187, 0, 0, 0, 119, 1, 0,
        2, 0, 16, 0, 100, 97, 116, 97,
    ],
];

/// Package i16 LE PCM data into a WAV container.
pub fn package(pcm: &[i16]) -> Vec<u8> {
    let mut data = Vec::with_capacity(HEADER_SIZE + pcm.len() * 2);
    write_header(&mut data, pcm.len() * 2);
    data.extend(pcm.iter().flat_map(|n| n.to_le_bytes()));
    data
}

/// Package i16 LE PCM data into a WAV container by prepending the buffer with a
/// header.
#[allow(dead_code)]
pub fn package_mut_raw(data: &mut Vec<u8>) {
    data.reserve_exact(HEADER_SIZE);
    write_header(data, data.len());
    data.rotate_right(HEADER_SIZE);
}

/// `pcm_len` being the number of bytes of the PCM payload.
fn write_header(buffer: &mut Vec<u8>, pcm_len: usize) {
    buffer.extend_from_slice(HEADER_TEMPLATES[0]);
    buffer.extend_from_slice(&((pcm_len + HEADER_SIZE - 8) as u32).to_le_bytes()); // Total length without data up to this point
    buffer.extend_from_slice(HEADER_TEMPLATES[1]);
    buffer.extend_from_slice(&((pcm_len as u32).to_le_bytes())); // PCM data length
}

// TODO: use Bytes to remove usage of rotate_left while keeping AsRef<u8> impl.
/// Remove the WAV header while keeping its payload unchanged (little endian).
/// Panics if the vec is not long enough to have PCM data.
#[allow(dead_code)]
pub fn remove_header(wav: &mut Vec<u8>) {
    wav.rotate_left(HEADER_SIZE);
    wav.truncate(wav.len() - HEADER_SIZE);
}

/// Validates that the data are a valid WAV containing PCM i16 LE data.
pub fn is_valid_pcm_s16le(data: &[u8]) -> bool {
    if data.len() < HEADER_SIZE || data.len() - HEADER_SIZE % 2 == 1 {
        return false;
    }
    &data[0..4] == HEADER_TEMPLATES[0]
        && data[4..8] == ((data.len() - 8) as u32).to_le_bytes()
        && &data[8..40] == HEADER_TEMPLATES[1]
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
        let pcm = iter::repeat_with(|| random::<i16>())
            .take(64 + random::<usize>() % 64)
            .collect_vec();

        // http://soundfile.sapp.org/doc/WaveFormat
        let mut data = Vec::with_capacity(HEADER_SIZE + pcm.len() * 2);
        data.extend_from_slice("RIFF".as_bytes());
        data.extend_from_slice(&((pcm.len() * 2 + HEADER_SIZE - 8) as u32).to_le_bytes()); // Total length without the data up to this point
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
        assert!(super::is_valid_pcm_s16le(&data));
        assert!(super::is_valid_pcm_s16le(&super::package(&pcm)));
    }

    #[test]
    fn package_mut_raw() {
        let pcm = iter::repeat_with(|| random::<i16>()).take(2).collect_vec();
        assert_eq!(super::package(&pcm), {
            let mut data = pcm.iter().flat_map(|n| n.to_le_bytes()).collect();
            super::package_mut_raw(&mut data);
            data
        });
    }

    #[test]
    fn remove_header() {
        let pcm = iter::repeat_with(|| random::<i16>())
            .take(64 + random::<usize>() % 64)
            .collect_vec();
        let mut wav = super::package(&pcm);

        let pcm = pcm.into_iter().flat_map(|n| n.to_le_bytes()).collect_vec();
        super::remove_header(&mut wav);
        assert_eq!(pcm, wav);
    }

    #[test]
    fn validate() {
        assert!(super::is_valid_pcm_s16le(include_bytes!("hello.wav")));
    }

    #[test]
    fn duration() {
        assert_eq!(
            super::duration_from_size(include_bytes!("hello.wav").len()),
            Duration::from_millis(1120)
        );
    }
}

//! Bounded GSR0146 prefix reader. Locate the serialized modding-state byte by
//! following counts, not a guessed absolute offset or a byte-pattern search.
use crate::isaac_format::{canonicalize_save, isaac_crc32};
use anyhow::{Context, Result, bail};

struct Reader<'a> {
    data: &'a [u8],
    position: usize,
}
impl Reader<'_> {
    fn skip(&mut self, length: usize) -> Result<()> {
        self.position = self
            .position
            .checked_add(length)
            .context("Continue offset overflow")?;
        if self.position > self.data.len().saturating_sub(4) {
            bail!("Continue prefix is truncated");
        }
        Ok(())
    }
    fn byte(&mut self) -> Result<u8> {
        let position = self.position;
        self.skip(1)?;
        Ok(self.data[position])
    }
    fn short(&mut self) -> Result<usize> {
        let position = self.position;
        self.skip(2)?;
        Ok(u16::from_le_bytes(self.data[position..self.position].try_into()?) as usize)
    }
    fn count(&mut self, maximum: usize) -> Result<usize> {
        let position = self.position;
        self.skip(4)?;
        let value = u32::from_le_bytes(self.data[position..self.position].try_into()?) as usize;
        if value > maximum {
            bail!(
                "Continue count at offset {position} exceeds its supported layout ({value} > {maximum})"
            );
        }
        Ok(value)
    }
    fn string(&mut self, maximum: usize) -> Result<()> {
        let size = self.short()?;
        if size > maximum {
            bail!("unsupported Continue string size");
        }
        self.skip(size)
    }
    fn entity(&mut self, depth: usize) -> Result<()> {
        if depth > 16 {
            bail!("Continue entity nesting exceeds safety limits");
        }
        self.skip(81)?;
        let flags = self.byte()?;
        if flags & !3 != 0 {
            bail!(
                "unsupported Continue entity flags at {} ({flags})",
                self.position - 1
            );
        }
        if flags & 1 != 0 {
            self.skip(32)?;
        }
        if flags & 2 != 0 {
            self.entity(depth + 1)?;
        }
        Ok(())
    }
    fn room(&mut self) -> Result<()> {
        // Config identity, room state/doors/seeds, then 448 grid entries.
        self.skip(12 + 7 + 4 + 32 + 12 + 4 + 24)?;
        for _ in 0..448 {
            if self.short()? != 0 {
                self.skip(22)?;
            }
        }
        let count = self.count(100_000)?;
        for _ in 0..count {
            self.entity(0)?;
        }
        let removed = self.count(100_000)?;
        self.skip(removed * 4)?;
        let count = self.count(100_000)?;
        for _ in 0..count {
            self.entity(0)?;
        }
        self.skip(2 + 2 + 16 + 8 + 4 + 2)
    }
}

pub fn modding_state(input: &[u8]) -> Result<Option<(usize, bool)>> {
    let bytes = crate::run_state::canonicalize(input)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let mut reader = Reader {
        data: &bytes,
        position: 16,
    };
    reader.skip(4 + 3 + 4)?;
    reader.string(49)?;
    reader.skip(52 + 12 + 1 + 4 + 272 + 32 + 4)?;
    reader.string(24)?;
    reader.skip(4 + 48 + 12 + 56)?;
    let configurations = reader.count(100_000)?;
    reader.skip(configurations * 12)?;
    let rooms = reader.count(506)?;
    for index in 0..rooms + 18 {
        reader
            .room()
            .with_context(|| format!("Continue room {index}"))?;
    }
    let pool = reader.short()?;
    if pool > 4096 {
        bail!("unsupported Continue item-pool length");
    }
    reader.skip(pool * 2 + 66)?;
    let offset = reader.position;
    let value = reader.byte()?;
    if value > 1 {
        bail!("invalid serialized modding state");
    }
    Ok(Some((offset, value != 0)))
}

pub struct AdaptedPair {
    pub progress: Vec<u8>,
    pub run: Vec<u8>,
}

/// Change only the native mod-state marker and the three checksums linked to
/// it. No item IDs, room data, player data, or mod content is translated.
pub fn adapt_pair(progress: &[u8], run: &[u8], target_modded: bool) -> Result<AdaptedPair> {
    crate::run_state::validate_pair(progress, run)?;
    let mut progress = canonicalize_save(progress)?.bytes;
    let mut run = crate::run_state::canonicalize(run)?;
    if let Some((offset, original)) = modding_state(&run)?
        && original != target_modded
    {
        run[offset] = u8::from(target_modded);
        let length = run.len();
        let crc = isaac_crc32(&run[16..length - 4], 0xfedc_ba76);
        run[length - 4..].copy_from_slice(&(crc ^ 0x9669_6996).to_le_bytes());
        progress[16..20].copy_from_slice(&crc.to_le_bytes());
        let length = progress.len();
        let crc = isaac_crc32(&progress[16..length - 4], 0xfedc_ba76);
        progress[length - 4..].copy_from_slice(&crc.to_le_bytes());
    }
    crate::run_state::validate_pair(&progress, &run)?;
    Ok(AdaptedPair { progress, run })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> AdaptedPair {
        let mut run = b"ISAACNG_GSR0146\0".to_vec();
        run.resize(16 + 11, 0);
        run.extend_from_slice(&0u16.to_le_bytes());
        run.resize(run.len() + 52 + 12 + 1 + 4 + 272 + 32 + 4, 0);
        run.extend_from_slice(&0u16.to_le_bytes());
        run.resize(run.len() + 4 + 48 + 12 + 56, 0);
        run.extend_from_slice(&0u32.to_le_bytes());
        run.extend_from_slice(&0u32.to_le_bytes());
        run.resize(run.len() + 18 * 1037, 0);
        run.extend_from_slice(&0u16.to_le_bytes());
        run.resize(run.len() + 66, 0);
        run.push(0);
        run.resize(run.len() + 128, 0);
        let crc = isaac_crc32(&run[16..], 0xfedc_ba76);
        run.extend_from_slice(&(crc ^ 0x9669_6996).to_le_bytes());
        let mut progress =
            crate::save_compat::tests::fixture(crate::save_compat::CloudVersion::Repentance);
        progress[16..20].copy_from_slice(&crc.to_le_bytes());
        let length = progress.len();
        let crc = isaac_crc32(&progress[16..length - 4], 0xfedc_ba76);
        progress[length - 4..].copy_from_slice(&crc.to_le_bytes());
        AdaptedPair { progress, run }
    }
    #[test]
    fn mod_state_roundtrip_changes_only_marker_and_linked_checksums() {
        let original = fixture();
        let (offset, modded) = modding_state(&original.run).unwrap().unwrap();
        assert!(!modded);
        let pc = adapt_pair(&original.progress, &original.run, true).unwrap();
        assert!(modding_state(&pc.run).unwrap().unwrap().1);
        for (index, (before, after)) in original.run.iter().zip(&pc.run).enumerate() {
            if before != after {
                assert!(index == offset || index >= original.run.len() - 4);
            }
        }
        for (index, (before, after)) in original.progress.iter().zip(&pc.progress).enumerate() {
            if before != after {
                assert!((16..20).contains(&index) || index >= original.progress.len() - 4);
            }
        }
        let ios = adapt_pair(&pc.progress, &pc.run, false).unwrap();
        assert_eq!(original.progress, ios.progress);
        assert_eq!(original.run, ios.run);
        crate::run_state::validate_pair(&pc.progress, &pc.run).unwrap();
        assert_eq!(
            crate::run_state::bundle_hash(
                &crate::local::identity_for_bytes(&original.progress).sha256,
                &original.run
            ),
            crate::run_state::bundle_hash(
                &crate::local::identity_for_bytes(&ios.progress).sha256,
                &ios.run
            )
        );
        let mut bad = pc.run;
        bad[offset] ^= 1;
        assert!(adapt_pair(&pc.progress, &bad, false).is_err());
    }
    #[test]
    fn malformed_counts_and_deep_entities_fail_closed() {
        let mut reader = Reader {
            data: &[255; 64],
            position: 16,
        };
        assert!(reader.count(506).is_err());
        assert!(reader.skip(usize::MAX).is_err());
        let mut nested = vec![];
        for _ in 0..18 {
            nested.resize(nested.len() + 81, 0);
            nested.push(2);
        }
        nested.extend_from_slice(&[0; 4]);
        let mut reader = Reader {
            data: &nested,
            position: 0,
        };
        assert!(reader.entity(0).is_err());
    }
    #[test]
    #[ignore = "requires private real GSR0146 fixtures"]
    fn private_modding_state_prefixes() {
        for key in ["ISAAC_IOS_RUN_FIXTURE", "ISAAC_DESKTOP_RUN_FIXTURE"] {
            let raw = std::fs::read(std::env::var(key).unwrap()).unwrap();
            eprintln!("{key}: {:?}", modding_state(&raw).unwrap());
            let progress_key = key.replace("RUN_FIXTURE", "PROGRESS_FIXTURE");
            if let Ok(path) = std::env::var(progress_key) {
                let original = canonicalize_save(&std::fs::read(path).unwrap())
                    .unwrap()
                    .bytes;
                let run = crate::run_state::canonicalize(&raw).unwrap();
                let (_, was_modded) = modding_state(&run).unwrap().unwrap();
                let changed = adapt_pair(&original, &run, !was_modded).unwrap();
                let restored = adapt_pair(&changed.progress, &changed.run, was_modded).unwrap();
                assert_eq!(original, restored.progress);
                assert_eq!(run, restored.run);
                assert_eq!(modding_state(&changed.run).unwrap().unwrap().1, !was_modded);
            }
        }
    }
}

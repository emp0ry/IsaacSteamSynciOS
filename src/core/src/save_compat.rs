//! Lossless adaptation of the known Repentance and Repentance+ progression layouts.
//!
//! The format differences were identified in n1cknam3l3ss/IsaacSteamSynciOS.
//! Unlike an offset-based splice, every input is CRC-validated and section
//! lengths determine all boundaries. Unknown layouts are refused. Rep+ fields
//! are preserved verbatim; conversion never invents achievement unlocks.
use crate::isaac_format::{canonicalize_save, isaac_crc32};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const REP_ACHIEVEMENTS: usize = 638;
const REP_COUNTERS: usize = 496;
const PLUS_ACHIEVEMENTS: usize = 642;
const PLUS_COUNTERS: usize = 523;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloudVersion {
    #[default]
    Auto,
    Repentance,
    RepentancePlus,
}

impl CloudVersion {
    pub fn from_filename(name: &str) -> Option<(Self, u8)> {
        let name = name.to_ascii_lowercase();
        for slot in 1..=3 {
            if name == format!("rep+persistentgamedata{slot}.dat") {
                return Some((Self::RepentancePlus, slot));
            }
            if name == format!("rep_persistentgamedata{slot}.dat")
                || name == format!("persistentgamedata{slot}.dat")
            {
                return Some((Self::Repentance, slot));
            }
        }
        None
    }

    pub fn filename(self, slot: u8) -> Result<String> {
        if !(1..=3).contains(&slot) {
            bail!("invalid save slot");
        }
        match self {
            Self::Repentance => Ok(format!("rep_persistentgamedata{slot}.dat")),
            Self::RepentancePlus => Ok(format!("rep+persistentgamedata{slot}.dat")),
            Self::Auto => bail!("choose a Steam save version before creating a cloud file"),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepPlusExtra {
    pub achievements: [u8; 4],
    pub counters: [u32; 27],
}

impl RepPlusExtra {
    fn validate(&self) -> Result<()> {
        if self.achievements.iter().any(|value| *value > 1) {
            bail!("invalid preserved Repentance+ achievement value");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompatibilityState {
    pub version: CloudVersion,
    // Stored in the account-bound sync state, not a global slot-only cache.
    pub rep_plus_extras: BTreeMap<u8, RepPlusExtra>,
    #[serde(default = "enabled_by_default")]
    pub auto_upload: bool,
    #[serde(default = "enabled_by_default")]
    pub transfer_runs: bool,
    #[serde(default)]
    pub pc_mods: bool,
}

fn enabled_by_default() -> bool {
    true
}
impl Default for CompatibilityState {
    fn default() -> Self {
        Self {
            version: CloudVersion::Auto,
            rep_plus_extras: BTreeMap::new(),
            auto_upload: true,
            transfer_runs: true,
            pc_mods: false,
        }
    }
}

pub struct ProjectedSave {
    pub bytes: Vec<u8>,
    pub version: CloudVersion,
    pub extra: Option<RepPlusExtra>,
}

struct Sections {
    version: CloudVersion,
    achievement_start: usize,
    counter_start: usize,
    counter_end: usize,
}

fn u32_at(bytes: &[u8], position: usize) -> Result<u32> {
    let value = bytes
        .get(position..position.checked_add(4).context("save offset overflow")?)
        .context("truncated compatibility section")?;
    Ok(u32::from_le_bytes(value.try_into()?))
}

fn sections(bytes: &[u8]) -> Result<Sections> {
    // canonicalize_save validates every section, trailer, size limit, and CRC.
    let achievements = u32_at(bytes, 28)? as usize;
    if u32_at(bytes, 20)? != 1 || u32_at(bytes, 24)? as usize != achievements {
        bail!("invalid achievement section for save conversion");
    }
    let counter_header = 32usize
        .checked_add(achievements)
        .context("save size overflow")?;
    let counters = u32_at(bytes, counter_header + 8)? as usize;
    if u32_at(bytes, counter_header)? != 2
        || u32_at(bytes, counter_header + 4)? as usize
            != counters.checked_mul(4).context("save size overflow")?
    {
        bail!("invalid counter section for save conversion");
    }
    let version = match (achievements, counters) {
        (REP_ACHIEVEMENTS, REP_COUNTERS) => CloudVersion::Repentance,
        (PLUS_ACHIEVEMENTS, PLUS_COUNTERS) => CloudVersion::RepentancePlus,
        _ => bail!(
            "unsupported save layout ({achievements} achievements, {counters} counters); no conversion performed"
        ),
    };
    let counter_start = counter_header + 12;
    let counter_end = counter_start
        .checked_add(counters * 4)
        .context("save size overflow")?;
    if counter_end > bytes.len().saturating_sub(8) {
        bail!("truncated save counter section");
    }
    Ok(Sections {
        version,
        achievement_start: 32,
        counter_start,
        counter_end,
    })
}

fn rebuild(
    bytes: &[u8],
    layout: &Sections,
    target: CloudVersion,
    extra: &RepPlusExtra,
) -> Result<Vec<u8>> {
    extra.validate()?;
    let plus = target == CloudVersion::RepentancePlus;
    let achievements = if plus {
        PLUS_ACHIEVEMENTS
    } else {
        REP_ACHIEVEMENTS
    };
    let counters = if plus { PLUS_COUNTERS } else { REP_COUNTERS };
    let mut output = bytes[..20].to_vec();
    for value in [1, achievements as u32, achievements as u32] {
        output.extend_from_slice(&value.to_le_bytes());
    }
    output.extend_from_slice(
        &bytes[layout.achievement_start..layout.achievement_start + REP_ACHIEVEMENTS],
    );
    if plus {
        output.extend_from_slice(&extra.achievements);
    }
    for value in [2, (counters * 4) as u32, counters as u32] {
        output.extend_from_slice(&value.to_le_bytes());
    }
    output.extend_from_slice(&bytes[layout.counter_start..layout.counter_start + REP_COUNTERS * 4]);
    if plus {
        for value in extra.counters {
            output.extend_from_slice(&value.to_le_bytes());
        }
    }
    // All other sections and the four non-CRC trailer bytes are unchanged.
    output.extend_from_slice(&bytes[layout.counter_end..bytes.len() - 4]);
    let crc = isaac_crc32(&output[16..], 0xfedc_ba76);
    output.extend_from_slice(&crc.to_le_bytes());
    canonicalize_save(&output).context("converted save failed structural/CRC verification")?;
    Ok(output)
}

pub fn project_for_ios(input: &[u8]) -> Result<ProjectedSave> {
    let canonical = canonicalize_save(input)?;
    let data = canonical.bytes;
    let layout = sections(&data)?;
    if layout.version == CloudVersion::Repentance {
        return Ok(ProjectedSave {
            bytes: data,
            version: layout.version,
            extra: None,
        });
    }
    let mut extra = RepPlusExtra::default();
    extra.achievements.copy_from_slice(
        &data[layout.achievement_start + REP_ACHIEVEMENTS..layout.counter_start - 12],
    );
    for (index, value) in extra.counters.iter_mut().enumerate() {
        *value = u32_at(&data, layout.counter_start + (REP_COUNTERS + index) * 4)?;
    }
    extra.validate()?;
    let bytes = rebuild(&data, &layout, CloudVersion::Repentance, &extra)?;
    Ok(ProjectedSave {
        bytes,
        version: layout.version,
        extra: Some(extra),
    })
}

pub fn prepare_for_steam(
    input: &[u8],
    target: CloudVersion,
    extra: Option<&RepPlusExtra>,
) -> Result<Vec<u8>> {
    if target == CloudVersion::Auto {
        bail!("Steam save version has not been selected");
    }
    let canonical = canonicalize_save(input)?;
    let layout = sections(&canonical.bytes)?;
    if layout.version != CloudVersion::Repentance {
        bail!("local iOS save is not standard Repentance");
    }
    if target == CloudVersion::Repentance {
        return Ok(canonical.bytes);
    }
    rebuild(
        &canonical.bytes,
        &layout,
        target,
        extra.unwrap_or(&RepPlusExtra::default()),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) fn fixture(version: CloudVersion) -> Vec<u8> {
        let mut data = b"ISAACNGSAVE09R  ".to_vec();
        data.extend_from_slice(&1234u32.to_le_bytes());
        let plus = version == CloudVersion::RepentancePlus;
        let ach = if plus {
            PLUS_ACHIEVEMENTS
        } else {
            REP_ACHIEVEMENTS
        };
        let counters = if plus { PLUS_COUNTERS } else { REP_COUNTERS };
        for value in [1, ach as u32, ach as u32] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        data.extend((0..ach).map(|index| (index % 7 == 0) as u8));
        for value in [2, (counters * 4) as u32, counters as u32] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        for index in 0..counters {
            data.extend_from_slice(&(index as u32 * 731).to_le_bytes());
        }
        for section in 3u32..=10 {
            let size = if [3, 8, 9].contains(&section) {
                4u32
            } else {
                1
            };
            for value in [section, size, 1] {
                data.extend_from_slice(&value.to_le_bytes());
            }
            data.extend(vec![section as u8; size as usize]);
        }
        for value in [11u32, 0, 4, 4, 0, 2, 0, 3, 0, 1, 0] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        data.extend_from_slice(&[0; 8]);
        crate::isaac_format::write_valid_checksum_for_tests(&mut data);
        data
    }
    #[test]
    fn exact_roundtrip_and_unchanged_representation() {
        let plus = fixture(CloudVersion::RepentancePlus);
        let projected = project_for_ios(&plus).unwrap();
        assert_eq!(projected.version, CloudVersion::RepentancePlus);
        assert_eq!(
            prepare_for_steam(
                &projected.bytes,
                CloudVersion::RepentancePlus,
                projected.extra.as_ref()
            )
            .unwrap(),
            plus
        );
        assert_eq!(
            prepare_for_steam(&projected.bytes, CloudVersion::Repentance, None).unwrap(),
            projected.bytes
        );
        assert!(project_for_ios(&projected.bytes).unwrap().extra.is_none());
    }
    #[test]
    fn does_not_unlock_read_up_or_merge_steam_achievements() {
        let mut rep = fixture(CloudVersion::Repentance);
        rep[33] = 1;
        crate::isaac_format::write_valid_checksum_for_tests(&mut rep);
        let plus = prepare_for_steam(&rep, CloudVersion::RepentancePlus, None).unwrap();
        assert_eq!(
            project_for_ios(&plus).unwrap().extra.unwrap(),
            RepPlusExtra::default()
        );
    }
    #[test]
    fn refuses_unknown_layouts_corruption_and_invalid_preserved_state() {
        let mut rep = fixture(CloudVersion::Repentance);
        rep[24..28].copy_from_slice(&637u32.to_le_bytes());
        crate::isaac_format::write_valid_checksum_for_tests(&mut rep);
        assert!(project_for_ios(&rep).is_err());
        let mut extra = RepPlusExtra::default();
        extra.achievements[0] = 2;
        assert!(
            prepare_for_steam(
                &fixture(CloudVersion::Repentance),
                CloudVersion::RepentancePlus,
                Some(&extra)
            )
            .is_err()
        );
        let mut plus = fixture(CloudVersion::RepentancePlus);
        plus[80] ^= 1;
        assert!(project_for_ios(&plus).is_err());
    }
    #[test]
    fn names_are_exact_and_exclude_runs_and_other_app_files() {
        assert_eq!(
            CloudVersion::from_filename("rep+persistentgamedata3.dat"),
            Some((CloudVersion::RepentancePlus, 3))
        );
        for name in [
            "rep+persistentgamedata4.dat",
            "rep+gamestate1.dat",
            "xrep+persistentgamedata1.dat",
            "../rep+persistentgamedata1.dat",
        ] {
            assert_eq!(CloudVersion::from_filename(name), None);
        }
    }
    #[test]
    #[ignore = "requires a private real Repentance+ fixture"]
    fn private_rep_plus_roundtrip_preserves_every_byte() {
        let path = std::env::var("ISAAC_REP_PLUS_FIXTURE")
            .expect("set a private fixture path outside the repository");
        let raw = std::fs::read(path).unwrap();
        let canonical = canonicalize_save(&raw).unwrap();
        let projected = project_for_ios(&raw).unwrap();
        assert_eq!(projected.version, CloudVersion::RepentancePlus);
        let rebuilt = prepare_for_steam(
            &projected.bytes,
            CloudVersion::RepentancePlus,
            projected.extra.as_ref(),
        )
        .unwrap();
        assert_eq!(rebuilt, canonical.bytes);
        eprintln!(
            "Real Rep+ roundtrip: {} bytes -> {} iOS bytes -> identical {} Steam bytes",
            canonical.bytes.len(),
            projected.bytes.len(),
            rebuilt.len()
        );
    }
}

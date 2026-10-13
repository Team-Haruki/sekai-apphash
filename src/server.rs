use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A Project SEKAI game server region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerRegion {
    Jp,
    En,
    Tw,
    Kr,
    Cn,
}

impl ServerRegion {
    pub const ALL: [Self; 5] = [Self::Jp, Self::En, Self::Tw, Self::Kr, Self::Cn];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Jp => "jp",
            Self::En => "en",
            Self::Tw => "tw",
            Self::Kr => "kr",
            Self::Cn => "cn",
        }
    }

    /// Colorful Palette operates JP and EN; the other regions run on Nuverse.
    #[must_use]
    pub const fn is_cp(self) -> bool {
        matches!(self, Self::Jp | Self::En)
    }

    /// The Android package name each region publishes under.
    #[must_use]
    pub const fn package_name(self) -> &'static str {
        match self {
            Self::Jp => "com.sega.pjsekai",
            Self::En => "com.sega.ColorfulStage.en",
            Self::Tw => "com.hermes.mk.asia",
            Self::Kr => "com.pjsekai.kr",
            Self::Cn => "com.hermes.mk",
        }
    }

    #[must_use]
    pub fn from_package(package: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|region| region.package_name() == package)
    }
}

impl fmt::Display for ServerRegion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ServerRegion {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "jp" => Ok(Self::Jp),
            "en" => Ok(Self::En),
            "tw" => Ok(Self::Tw),
            "kr" => Ok(Self::Kr),
            "cn" => Ok(Self::Cn),
            other => Err(format!(
                "unknown region {other:?} (expected jp, en, tw, kr or cn)"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats_every_region() {
        for region in ServerRegion::ALL {
            assert_eq!(region.as_str().parse::<ServerRegion>(), Ok(region));
            assert_eq!(region.to_string(), region.as_str());
            assert_eq!(
                ServerRegion::from_package(region.package_name()),
                Some(region)
            );
        }
        assert_eq!("JP".parse::<ServerRegion>(), Ok(ServerRegion::Jp));
        assert!("xx".parse::<ServerRegion>().is_err());
        assert_eq!(ServerRegion::from_package("com.example"), None);
    }

    #[test]
    fn only_jp_and_en_are_colorful_palette() {
        let cp: Vec<_> = ServerRegion::ALL
            .into_iter()
            .filter(|r| r.is_cp())
            .collect();
        assert_eq!(cp, vec![ServerRegion::Jp, ServerRegion::En]);
    }
}

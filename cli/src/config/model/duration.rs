//! Durations in the host file: a positive whole number and one unit (`"500ms"`, `"10s"`, `"10m"`, `"1h"`), and
//! [`Timeout`] for a wait that may also be `"never"`. Written back in the largest unit that's exact, so `600000` ms
//! reads `"10m"`.

use std::{fmt, str::FromStr, time::Duration};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A positive length of time, as the host file writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigDuration(Duration);

/// Why a duration's text can't be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DurationError {
    #[error("{text:?} isn't a duration: write a whole number and one unit (ms, s, m, or h), like \"10s\"")]
    Format { text: String },
    #[error("{text:?} is zero: a duration must be longer than that")]
    Zero { text: String },
    #[error("{text:?} is too long")]
    TooLong { text: String },
}

/// The units, largest first: the order [`ConfigDuration`]'s text picks the largest exact one in.
const UNITS: [(&str, u64); 4] = [("h", 3_600_000), ("m", 60_000), ("s", 1_000), ("ms", 1)];

impl ConfigDuration {
    pub const fn from_millis(ms: u64) -> Self {
        Self(Duration::from_millis(ms))
    }

    pub fn get(self) -> Duration {
        self.0
    }
}

impl FromStr for ConfigDuration {
    type Err = DurationError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let format = || DurationError::Format { text: text.into() };
        let split = text.find(|c: char| !c.is_ascii_digit()).ok_or_else(format)?;
        let (number, unit) = text.split_at(split);
        if number.is_empty() {
            return Err(format());
        }
        let scale = UNITS
            .iter()
            .find(|(name, _)| *name == unit)
            .map(|(_, scale)| *scale)
            .ok_or_else(format)?;
        let count: u64 = number
            .parse()
            .map_err(|_| DurationError::TooLong { text: text.into() })?;
        let ms = count
            .checked_mul(scale)
            .ok_or_else(|| DurationError::TooLong { text: text.into() })?;
        match ms {
            0 => Err(DurationError::Zero { text: text.into() }),
            ms => Ok(Self::from_millis(ms)),
        }
    }
}

impl fmt::Display for ConfigDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = u64::try_from(self.0.as_millis()).unwrap_or(u64::MAX);
        let (unit, scale) = UNITS
            .iter()
            .find(|(_, scale)| ms % scale == 0)
            .copied()
            .unwrap_or(("ms", 1));
        write!(f, "{}{unit}", ms / scale)
    }
}

impl Serialize for ConfigDuration {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ConfigDuration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// How long something waits: a duration, or `"never"` for no limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timeout {
    After(ConfigDuration),
    Never,
}

const NEVER: &str = "never";

impl Timeout {
    /// The deadline from `now`; none for [`Timeout::Never`].
    pub fn deadline(self, now: tokio::time::Instant) -> Option<tokio::time::Instant> {
        match self {
            Timeout::After(duration) => Some(now + duration.get()),
            Timeout::Never => None,
        }
    }
}

impl FromStr for Timeout {
    type Err = DurationError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            NEVER => Ok(Timeout::Never),
            text => text.parse().map(Timeout::After),
        }
    }
}

impl fmt::Display for Timeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Timeout::After(duration) => duration.fmt(f),
            Timeout::Never => f.write_str(NEVER),
        }
    }
}

impl Serialize for Timeout {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Timeout {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_and_print_in_the_largest_exact_unit() {
        for (text, ms, printed) in [
            ("500ms", 500, "500ms"),
            ("10s", 10_000, "10s"),
            ("10m", 600_000, "10m"),
            ("1h", 3_600_000, "1h"),
            ("90s", 90_000, "90s"),
            ("60s", 60_000, "1m"),
            ("1500ms", 1_500, "1500ms"),
        ] {
            let duration: ConfigDuration = text.parse().unwrap();
            assert_eq!(duration, ConfigDuration::from_millis(ms), "{text}");
            assert_eq!(duration.to_string(), printed, "{text}");
        }
    }

    #[test]
    fn other_texts_are_refused() {
        for text in [
            "", "10", "s", "1m30s", "1.5s", "-1s", " 10s", "10 s", "10S", "10d", "never",
        ] {
            assert!(
                matches!(text.parse::<ConfigDuration>(), Err(DurationError::Format { .. })),
                "{text:?}"
            );
        }
        assert!(matches!(
            "0s".parse::<ConfigDuration>(),
            Err(DurationError::Zero { .. })
        ));
        assert!(matches!(
            "99999999999999999999h".parse::<ConfigDuration>(),
            Err(DurationError::TooLong { .. })
        ));
        assert!(matches!(
            "18446744073709551h".parse::<ConfigDuration>(),
            Err(DurationError::TooLong { .. })
        ));
    }

    #[test]
    fn a_timeout_may_be_never() {
        assert_eq!("never".parse::<Timeout>(), Ok(Timeout::Never));
        assert_eq!(
            "10m".parse::<Timeout>(),
            Ok(Timeout::After(ConfigDuration::from_millis(600_000)))
        );
        assert!("0s".parse::<Timeout>().is_err());
        assert_eq!(Timeout::Never.to_string(), "never");
        let now = tokio::time::Instant::now();
        assert_eq!(Timeout::Never.deadline(now), None);
        assert_eq!(
            Timeout::After(ConfigDuration::from_millis(5)).deadline(now),
            Some(now + Duration::from_millis(5))
        );
    }

    #[test]
    fn toml_values_are_strings() {
        #[derive(Deserialize, Serialize)]
        struct File {
            wait: ConfigDuration,
            timeout: Timeout,
        }
        let file: File = toml::from_str("wait = \"3s\"\ntimeout = \"never\"\n").unwrap();
        assert_eq!(file.wait, ConfigDuration::from_millis(3_000));
        assert_eq!(toml::to_string(&file).unwrap(), "wait = \"3s\"\ntimeout = \"never\"\n");
        assert!(toml::from_str::<File>("wait = 3000\ntimeout = \"never\"\n").is_err());
    }
}
